/// Chromium backend: acts as a WebSocket server that the Canon Chrome extension
/// connects to. Prompts are injected into a live ChatGPT tab via the extension
/// relay; SSE frames come back as INBOUND_MESSAGE and are assembled by the
/// same `parsers::FrameAssembler` used in canon-llm-runtime.
///
/// This backend does not talk directly to the OpenAI API. It depends on a
/// browser session that is already authenticated to ChatGPT so the extension
/// can either claim an existing ChatGPT tab or open one and submit prompts
/// into that page.
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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const FRAMES_DIR: &str = "./frames";
const AGENT_STATE_DIR: &str = "./agent_state";
const PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD: u32 = 8;
const CHROMIUM_AUTOLAUNCH_DISABLE_ENV: &str = "CANON_CHROMIUM_AUTOLAUNCH";
const CHROMIUM_AUTOLAUNCH_SCRIPT_ENV: &str = "CANON_CHROMIUM_LAUNCH_SCRIPT";

static CHROMIUM_AUTOLAUNCH_ATTEMPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

static EXTENSION_CONN_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn chromium_autolaunch_log_path() -> PathBuf {
    Path::new(AGENT_STATE_DIR).join("chromium-autolaunch.log")
}

fn chromium_autolaunch_script_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(CHROMIUM_AUTOLAUNCH_SCRIPT_ENV) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    let default = PathBuf::from("./canon-chromium-extension/launch_chromium.sh");
    default.exists().then_some(default)
}

fn chromium_autolaunch_enabled() -> bool {
    match std::env::var(CHROMIUM_AUTOLAUNCH_DISABLE_ENV) {
        Ok(raw) => !matches!(raw.trim(), "0" | "false" | "FALSE" | "off" | "OFF"),
        Err(_) => true,
    }
}

fn maybe_autolaunch_chromium() {
    if !chromium_autolaunch_enabled() {
        return;
    }
    if CHROMIUM_AUTOLAUNCH_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(script_path) = chromium_autolaunch_script_path() else {
        eprintln!(
            "[chromium] autolaunch skipped; no launch script found (set {CHROMIUM_AUTOLAUNCH_SCRIPT_ENV} to override)"
        );
        return;
    };

    let _ = std::fs::create_dir_all(AGENT_STATE_DIR);
    let log_path = chromium_autolaunch_log_path();
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path);

    let mut cmd = Command::new("/bin/bash");
    cmd.arg(&script_path)
        .current_dir(".")
        .stdin(Stdio::null())
        .stdout(stdout.map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .stderr(stderr.map(Stdio::from).unwrap_or_else(|_| Stdio::null()));

    match cmd.spawn() {
        Ok(child) => {
            eprintln!(
                "[chromium] autolaunch started pid={} script={} log={}",
                child.id(),
                script_path.display(),
                log_path.display()
            );
        }
        Err(err) => {
            eprintln!(
                "[chromium] autolaunch failed for {}: {err:#}",
                script_path.display()
            );
        }
    }
}

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

fn extract_calpico_user_message_trigger_id(raw: &str) -> Option<String> {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    let value: Value = serde_json::from_str(data).ok()?;
    let envelopes = value.as_array()?;
    for envelope in envelopes {
        if envelope.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let payload = envelope.get("payload")?;
        if payload.get("type").and_then(Value::as_str) != Some("calpico-message-add") {
            continue;
        }
        let message = payload.get("payload")?.get("message")?;
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let message_id = message.get("id").and_then(Value::as_str)?;
        return Some(
            message_id
                .rsplit('~')
                .next()
                .unwrap_or(message_id)
                .to_string(),
        );
    }
    None
}

fn extract_calpico_heartbeat_trigger_message_id(raw: &str) -> Option<String> {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    let value: Value = serde_json::from_str(data).ok()?;
    let envelopes = value.as_array()?;
    for envelope in envelopes {
        if envelope.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let payload = envelope.get("payload")?;
        if payload.get("type").and_then(Value::as_str)
            != Some("calpico-is-responding-heartbeat")
        {
            continue;
        }
        let trigger_message_id = payload
            .get("payload")?
            .get("source")?
            .get("trigger_message_id")?
            .as_str()?;
        return Some(trigger_message_id.to_string());
    }
    None
}

fn extract_calpico_heartbeat_progress_signature(raw: &str) -> Option<String> {
    let data = raw.strip_prefix("data: ").unwrap_or(raw).trim();
    let value: Value = serde_json::from_str(data).ok()?;
    let envelopes = value.as_array()?;
    for envelope in envelopes {
        if envelope.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let payload = envelope.get("payload")?;
        if payload.get("type").and_then(Value::as_str)
            != Some("calpico-is-responding-heartbeat")
        {
            continue;
        }
        let source = payload.get("payload")?.get("source")?;
        let trigger_message_id = source
            .get("trigger_message_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let task_type = source.get("task_type").and_then(Value::as_str).unwrap_or("");
        let message = source.get("message").and_then(Value::as_str).unwrap_or("");
        return Some(format!(
            "trigger={trigger_message_id}|task={task_type}|message={message}"
        ));
    }
    None
}


// ---------------------------------------------------------------------------
// Shared server state (guarded by a single Mutex)
// ---------------------------------------------------------------------------

struct State {
    /// Channel to send frames to the primary (first-connected) extension.
    /// Replaced only when the primary disconnects; a second connection is held as standby.
    out_tx: Option<mpsc::Sender<Message>>,

    /// Connection ID of the current primary, used to distinguish primary from standby on disconnect.
    primary_conn_id: Option<u64>,

    /// Sender for a second extension that connected while a primary was already active.
    /// Promoted to primary when the primary disconnects.
    standby_tx: Option<(u64, mpsc::Sender<Message>)>,

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

    /// Tabs retired after an early transport failure/timeout and awaiting
    /// `TAB_CLOSED`. These must never be reclaimed by reconciliation because
    /// their live browser state is known-bad for the next turn.
    quarantined_tabs: HashSet<u32>,

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

    /// (tabId, turnId) -> trigger message id derived from the echoed user
    /// prompt. Responding heartbeats for any other trigger id are stale
    /// cross-turn noise and must not advance the current turn's stall counter.
    user_message_trigger_id: HashMap<(u32, u64), String>,

    /// (tabId, turnId) -> count of non-progress `heartbeat` frames observed
    /// after the user prompt echo but before any assistant progress or
    /// `turn_complete`.
    post_user_message_heartbeat: HashMap<(u32, u64), u32>,

    /// (tabId, turnId) -> last observed responding-heartbeat progress
    /// signature for the current trigger id. Identical repeated signatures are
    /// stall evidence; signature changes are treated as forward progress.
    post_user_message_heartbeat_signature: HashMap<(u32, u64), String>,

    /// (tabId, turnId) keys that have already consumed the one-time grace for
    /// the first responding heartbeat after a user echo.
    post_user_message_first_heartbeat_graced: HashSet<(u32, u64)>,
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
            primary_conn_id: None,
            standby_tx: None,
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
            quarantined_tabs: HashSet::new(),
            replay_queue: Vec::new(),
            frame_counter: 0,
            turn_complete_seen: HashMap::new(),
            post_complete_presence: HashMap::new(),
            post_complete_heartbeat: HashMap::new(),
            user_message_seen: HashMap::new(),
            user_message_trigger_id: HashMap::new(),
            post_user_message_heartbeat: HashMap::new(),
            post_user_message_heartbeat_signature: HashMap::new(),
            post_user_message_first_heartbeat_graced: HashSet::new(),
        }
    }

    fn send_msg(&self, msg: Value) -> bool {
        if let Some(tx) = &self.out_tx {
            tx.try_send(Message::Text(msg.to_string().into())).is_ok()
        } else {
            false
        }
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

        maybe_autolaunch_chromium();

        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        tokio::spawn(accept_loop(addr, state));

        backend
    }

    /// Block until the Chrome extension has connected.
    pub async fn wait_for_connection(&self) {
        loop {
            {
                let st = self.state.lock().await;
                if st.out_tx.is_some() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Get any available tab from the preopened pool.
    async fn pop_any_tab(&self) -> Option<u32> {
        let mut st = self.state.lock().await;
        let quarantined_tabs = st.quarantined_tabs.clone();
        let mut claimed = None;
        for queue in st.preopened.values_mut() {
            while let Some(tab_id) = queue.pop_front() {
                if quarantined_tabs.contains(&tab_id) {
                    continue;
                }
                claimed = Some(tab_id);
                break;
            }
            if claimed.is_some() {
                break;
            }
        }
        let tab_id = claimed?;
        let url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
        st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
        Some(tab_id)
    }

    async fn pop_matching_url_tab(&self, urls: &[String]) -> Option<u32> {
        let mut st = self.state.lock().await;
        let quarantined_tabs = st.quarantined_tabs.clone();
        for url in urls {
            let Some(queue) = st.preopened.get_mut(url) else {
                continue;
            };
            let mut tab_id = None;
            while let Some(candidate) = queue.pop_front() {
                if quarantined_tabs.contains(&candidate) {
                    continue;
                }
                tab_id = Some(candidate);
                break;
            }
            let Some(tab_id) = tab_id else {
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
        maybe_autolaunch_chromium();

        // Wait for extension to connect first (up to timeout).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if self.state.lock().await.out_tx.is_some() {
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
                        // Owned stateful tabs remain bound to the endpoint across
                        // a soft-reset quarantine so acquisition waits for the
                        // original tab instead of opening a duplicate.
                        if st.quarantined_tabs.contains(&tab_id) {
                            // Stateful transport retirement soft-resets the owned tab in place.
                            // Wait for the TAB_READY reannouncement instead of opening a second
                            // tab for the same endpoint URL.
                        } else if let Some(url) = st.tab_urls.get(&tab_id).cloned() {
                            st.send_msg(json!({ "type": "CLAIM_TAB", "tabId": tab_id, "url": url }));
                            return Ok(tab_id);
                        } else {
                            st.endpoint_tabs.remove(endpoint_id);
                            st.tab_owners.remove(&tab_id);
                        }
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
            Response(String),
            ResponseDropped,
            EarlyFail(String),
            Timeout,
        }

        let mut resp_rx = resp_rx;
        let mut early_fail_rx = early_fail_rx;

        // Wait for the assembled response, but allow deterministic protocol
        // evidence (non-responding heartbeat stall) to fail the turn before the
        // wall-clock timeout expires.  Responding heartbeats are liveness
        // evidence and never trigger early_fail; the wall-clock timeout covers
        // the "ChatGPT is responding but too slow" case.
        let wait_outcome = loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break ResponseWaitOutcome::Timeout;
            }

            let wait_remaining = deadline.saturating_duration_since(now);
            let sleep = tokio::time::sleep(wait_remaining);
            tokio::pin!(sleep);

            tokio::select! {
                raw = &mut resp_rx => {
                    match raw {
                        Ok(text) => break ResponseWaitOutcome::Response(text),
                        Err(_) => break ResponseWaitOutcome::ResponseDropped,
                    }
                }
                early = &mut early_fail_rx => {
                    match early {
                        Ok(reason) => break ResponseWaitOutcome::EarlyFail(reason),
                        Err(_) => {
                            // Early-fail sender can disappear when the turn context is reset.
                            // Treat this as missing early-fail evidence and continue waiting.
                            continue;
                        }
                    }
                }
                _ = &mut sleep => {
                    break ResponseWaitOutcome::Timeout;
                }
            }
        };

        match wait_outcome {
            ResponseWaitOutcome::Response(raw) => {
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
            ResponseWaitOutcome::EarlyFail(reason) => {
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
            ResponseWaitOutcome::Timeout => {
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
            ResponseWaitOutcome::ResponseDropped => {
                let mut st = self.state.lock().await;
                append_inbound_boundary_event(
                    &mut st,
                    tab_id,
                    turn_id,
                    endpoint_id,
                    "response_channel_dropped",
                    "chromium: response channel dropped before completion",
                    stateful,
                    false,
                );
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_early_fail.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                st.pending_turn_lease.remove(&(tab_id, turn_id));
                retire_tab_locked(&mut st, endpoint_id, tab_id, stateful);
                append_outbound_event(
                    "OUTBOUND_RESPONSE_CHANNEL_DROPPED",
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
                    "chromium: response channel dropped before completion (tab={tab_id} turn={turn_id})"
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

    let my_id = EXTENSION_CONN_ID.fetch_add(1, Ordering::SeqCst);

    {
        let mut st = state.lock().await;
        if st.out_tx.is_some() {
            // A primary is already active. Hold this connection as standby so the
            // primary's command channel is not disrupted mid-turn.
            eprintln!("[chromium] second extension (conn={my_id}) connected, held as standby");
            st.standby_tx = Some((my_id, tx_out.clone()));
            // Do not replay the queue — the primary will handle those frames.
        } else {
            st.out_tx = Some(tx_out.clone());
            st.primary_conn_id = Some(my_id);
            eprintln!("[chromium] extension connected (conn={my_id})");
            for frame in st.replay_queue.drain(..) {
                let _ = tx_out.try_send(Message::Text(frame.to_string().into()));
            }
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

    {
        let mut st = state.lock().await;
        if st.primary_conn_id == Some(my_id) {
            // Primary disconnected — promote standby if one is waiting.
            if let Some((standby_id, standby_tx)) = st.standby_tx.take() {
                eprintln!(
                    "[chromium] primary (conn={my_id}) disconnected; promoting standby (conn={standby_id})"
                );
                st.out_tx = Some(standby_tx.clone());
                st.primary_conn_id = Some(standby_id);
                for frame in st.replay_queue.drain(..) {
                    let _ = standby_tx.try_send(Message::Text(frame.to_string().into()));
                }
            } else {
                eprintln!("[chromium] extension disconnected (conn={my_id})");
                st.out_tx = None;
                st.primary_conn_id = None;
            }
        } else {
            // Standby disconnected — just clear the standby slot if it matches.
            if st.standby_tx.as_ref().map(|(id, _)| *id == my_id).unwrap_or(false) {
                st.standby_tx = None;
            }
            eprintln!("[chromium] standby extension disconnected (conn={my_id})");
        }
    }
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
            let tab_owner = st.tab_owners.get(&tab_id).cloned();
            let site = SiteType::from_url(&url);
            st.assemblers
                .entry(tab_id)
                .and_modify(|a| a.set_site(site))
                .or_insert_with(|| FrameAssembler::new(site));
            st.tab_urls.insert(tab_id, url.clone());
            eprintln!("[chromium] TAB_READY tab={tab_id} url={url}");

            if st.quarantined_tabs.contains(&tab_id) {
                // Stateful tabs retired after a transport failure are soft-reset
                // with NAVIGATE_TAB. Their stale post-retirement traffic must stay
                // quarantined until the extension re-announces the tab as ready.
                // Once TAB_READY arrives for that same tab, promote it back into
                // normal reconciliation/reuse flow instead of leaving it stuck in
                // permanent quarantine.
                st.quarantined_tabs.remove(&tab_id);
            }

            if let Some(rid) = req_id {
                // This was a tab we opened via OPEN_TAB.
                if let Some(tx) = st.pending_open.remove(&rid) {
                    let _ = tx.send(tab_id);
                }
            } else if tab_owner.is_none() {
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
            st.quarantined_tabs.remove(&tab_id);
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
            st.user_message_trigger_id
                .retain(|(tid, _), _| *tid != tab_id);
            st.post_user_message_heartbeat
                .retain(|(tid, _), _| *tid != tab_id);
            st.post_user_message_heartbeat_signature
                .retain(|(tid, _), _| *tid != tab_id);
            st.post_user_message_first_heartbeat_graced
                .retain(|(tid, _)| *tid != tab_id);
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
            if st.quarantined_tabs.contains(&tab_id) {
                append_outbound_event(
                    "OUTBOUND_INBOUND_IGNORED",
                    json!({
                        "reason": "quarantined_tab_during_reset",
                        "tabId": tab_id,
                    }),
                );
                return;
            }
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
                    st.user_message_trigger_id.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    st.post_user_message_heartbeat_signature.remove(&key);
                    st.post_user_message_first_heartbeat_graced.remove(&key);
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
                    if let Some(trigger_message_id) =
                        extract_calpico_user_message_trigger_id(&chunk)
                    {
                        st.user_message_trigger_id.insert(key, trigger_message_id);
                    }
                    st.post_user_message_heartbeat.insert(key, 0);
                    st.post_user_message_heartbeat_signature.remove(&key);
                    st.post_user_message_first_heartbeat_graced.remove(&key);
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
                    st.user_message_trigger_id.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    st.post_user_message_heartbeat_signature.remove(&key);
                    st.post_user_message_first_heartbeat_graced.remove(&key);
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
                        if heartbeat_count >= 8 {
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
                        // Tabs can continue emitting responding-heartbeat frames for
                        // older prompt echoes after a turn has already been retired.
                        // Treat those as stale cross-turn noise rather than evidence
                        // about the current turn, otherwise the stall counter can be
                        // driven by a prior trigger_message_id and deterministically
                        // trip an early transport failure on a healthy new turn.
                        let expected_trigger_message_id =
                            st.user_message_trigger_id.get(&key).cloned();
                        let heartbeat_trigger_message_id =
                            extract_calpico_heartbeat_trigger_message_id(&chunk);
                        if let Some(expected_trigger_message_id) =
                            expected_trigger_message_id.as_deref()
                        {
                            match heartbeat_trigger_message_id.as_deref() {
                                Some(actual_trigger_message_id)
                                    if actual_trigger_message_id
                                        == expected_trigger_message_id => {}
                                Some(actual_trigger_message_id) => {
                                    append_outbound_event(
                                        "OUTBOUND_EARLY_SIGNAL",
                                        json!({
                                            "signal": "heartbeat_ignored_due_to_trigger_message_mismatch",
                                            "tabId": tab_id,
                                            "turnId": turn_id,
                                            "frame_counter": st.frame_counter,
                                            "expected_trigger_message_id": expected_trigger_message_id,
                                            "actual_trigger_message_id": actual_trigger_message_id,
                                            "user_message_frame_counter": user_message_frame,
                                        }),
                                    );
                                    return;
                                }
                                None => {
                                    append_outbound_event(
                                        "OUTBOUND_EARLY_SIGNAL",
                                        json!({
                                            "signal": "heartbeat_ignored_due_to_missing_trigger_message_id",
                                            "tabId": tab_id,
                                            "turnId": turn_id,
                                            "frame_counter": st.frame_counter,
                                            "expected_trigger_message_id": expected_trigger_message_id,
                                            "user_message_frame_counter": user_message_frame,
                                        }),
                                    );
                                    return;
                                }
                            }
                        }
                        let responding_heartbeat = chunk.contains("\"calpico-is-responding-heartbeat\"");
                        let heartbeat_progress_signature = if responding_heartbeat {
                            extract_calpico_heartbeat_progress_signature(&chunk)
                        } else {
                            None
                        };
                        let progress_updated = if let Some(signature) =
                            heartbeat_progress_signature.as_deref()
                        {
                            match st.post_user_message_heartbeat_signature.get(&key) {
                                Some(previous) if previous == signature => false,
                                _ => {
                                    st.post_user_message_heartbeat_signature
                                        .insert(key, signature.to_string());
                                    true
                                }
                            }
                        } else {
                            false
                        };
                        if progress_updated {
                            st.post_user_message_heartbeat.insert(key, 0);
                            append_outbound_event(
                                "OUTBOUND_EARLY_SIGNAL",
                                json!({
                                    "signal": "heartbeat_progress_update_after_user_echo",
                                    "tabId": tab_id,
                                    "turnId": turn_id,
                                    "frame_counter": st.frame_counter,
                                    "user_message_frame_counter": user_message_frame,
                                    "progress_signature": heartbeat_progress_signature,
                                }),
                            );
                            return;
                        }
                        let first_heartbeat_after_user_echo = st
                            .post_user_message_heartbeat
                            .get(&key)
                            .copied()
                            .unwrap_or(0)
                            == 0
                            && !st.post_user_message_first_heartbeat_graced.contains(&key);
                        if responding_heartbeat && first_heartbeat_after_user_echo {
                            st.post_user_message_first_heartbeat_graced.insert(key);
                            append_outbound_event(
                                "OUTBOUND_EARLY_SIGNAL",
                                json!({
                                    "signal": "first_responding_heartbeat_after_user_echo_grace",
                                    "tabId": tab_id,
                                    "turnId": turn_id,
                                    "frame_counter": st.frame_counter,
                                    "user_message_frame_counter": user_message_frame,
                                    "progress_signature": heartbeat_progress_signature,
                                }),
                            );
                            return;
                        }
                        let count = st
                            .post_user_message_heartbeat
                            .entry(key)
                            .and_modify(|seen| *seen += 1)
                            .or_insert(1);
                        let heartbeat_count = *count;
                        let signal = if responding_heartbeat {
                            "responding_heartbeat_after_user_echo_before_turn_complete"
                        } else {
                            "heartbeat_after_user_echo_before_turn_complete"
                        };
                        // Responding heartbeats are liveness evidence — ChatGPT is
                        // actively processing.  Only plain (non-responding) heartbeats
                        // indicate a genuine stall; the wall-clock timeout handles the
                        // "ChatGPT is responding but too slow" case.
                        if heartbeat_count >= PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD
                            && !responding_heartbeat
                        {
                            if let Some(tx) = st.pending_early_fail.remove(&key) {
                                let _ = tx.send(signal.to_string());
                            }
                        }
                        append_outbound_event(
                            "OUTBOUND_EARLY_SIGNAL",
                            json!({
                                "signal": signal,
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
                    st.user_message_trigger_id.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    st.post_user_message_heartbeat_signature.remove(&key);
                    st.post_user_message_first_heartbeat_graced.remove(&key);
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
                    st.user_message_trigger_id.remove(&key);
                    st.post_user_message_heartbeat.remove(&key);
                    st.post_user_message_heartbeat_signature.remove(&key);
                    st.post_user_message_first_heartbeat_graced.remove(&key);
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
    let close_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
    st.pending_turn_id.remove(&tab_id);
    st.pending_turn_lease.retain(|(tid, _), _| *tid != tab_id);
    st.pending_ack.retain(|(tid, _), _| *tid != tab_id);
    st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
    st.pending_early_fail.retain(|(tid, _), _| *tid != tab_id);
    st.turn_complete_seen.retain(|(tid, _), _| *tid != tab_id);
    st.post_complete_presence.retain(|(tid, _), _| *tid != tab_id);
    st.post_complete_heartbeat.retain(|(tid, _), _| *tid != tab_id);
    st.user_message_seen.retain(|(tid, _), _| *tid != tab_id);
    st.user_message_trigger_id
        .retain(|(tid, _), _| *tid != tab_id);
    st.post_user_message_heartbeat
        .retain(|(tid, _), _| *tid != tab_id);
    st.post_user_message_heartbeat_signature
        .retain(|(tid, _), _| *tid != tab_id);
    st.post_user_message_first_heartbeat_graced
        .retain(|(tid, _)| *tid != tab_id);
    st.assemblers.remove(&tab_id);

    if stateful {
        // Do not hard-close stateful ChatGPT/Gemini tabs on transport failure.
        // Closing guarantees the next acquire path has nothing reusable and
        // forces open-tab churn. Instead, soft-reset the tab by navigating it
        // back to its current provider URL without a reqId so the eventual
        // TAB_READY is treated as an organic reusable tab announcement.
        if !close_url.is_empty() {
            st.quarantined_tabs.insert(tab_id);
            eprintln!(
                "[chromium] soft-resetting stateful tab {tab_id} to {close_url} after transport retirement"
            );
            let _ = st.send_msg(json!({
                "type": "NAVIGATE_TAB",
                "tabId": tab_id,
                "url": close_url,
            }));
            for queue in st.preopened.values_mut() {
                queue.retain(|id| *id != tab_id);
            }
            return;
        }

        if st.endpoint_tabs.get(endpoint_id).copied() == Some(tab_id) {
            st.endpoint_tabs.remove(endpoint_id);
        }
        st.tab_owners.remove(&tab_id);
    }

    st.quarantined_tabs.insert(tab_id);
    let _ = st.send_msg(json!({ "type": "CLOSE_TAB", "tabId": tab_id, "url": close_url }));

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
    st.quarantined_tabs.retain(|tab_id| known_tab_ids.contains(tab_id));

    let owned_by_tab = st.tab_owners.clone();
    st.endpoint_tabs.retain(|endpoint, tab_id| {
        known_tab_ids.contains(tab_id)
            && owned_by_tab
                .get(tab_id)
                .map(String::as_str)
                == Some(endpoint.as_str())
    });

    let owned_tab_ids: HashSet<u32> = st.tab_owners.keys().copied().collect();
    let quarantined_tab_ids = st.quarantined_tabs.clone();
    let mut seen_preopened = HashSet::new();
    st.preopened.retain(|_, queue| {
        queue.retain(|tab_id| {
            known_tab_ids.contains(tab_id)
                && !owned_tab_ids.contains(tab_id)
                && !quarantined_tab_ids.contains(tab_id)
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
        if quarantined_tab_ids.contains(&tab_id) {
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
        endpoint_submit_ack_timeout_secs, handle_inbound, retire_tab_locked,
        PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD, State,
    };
    use crate::llm_runtime::parsers::{FrameAssembler, SiteType};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio::sync::{oneshot, Mutex};

    async fn recv_early_fail(early_fail_rx: oneshot::Receiver<String>, context: &str) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(1), early_fail_rx)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for deterministic early fail: {context}"))
            .unwrap_or_else(|_| panic!("early fail sender dropped before emitting reason: {context}"))
    }

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

        let reason = recv_early_fail(
            early_fail_rx,
            "assistant message add followed by repeated post-turn-complete heartbeats",
        )
        .await;
        assert_eq!(reason, "heartbeat_after_turn_complete");
    }

    // Responding heartbeats prove ChatGPT is alive and actively processing.
    // They must NOT trigger early_fail regardless of count; the wall-clock
    // timeout covers the "responding but too slow" case.
    #[tokio::test]
    async fn responding_heartbeats_after_user_echo_do_not_trigger_early_fail() {
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
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-message-add","payload":{"message":{"id":"room~room~CalpicoMessage~trigger-1","role":"user","content":{"text":"prompt"}}}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&user_echo, &state).await;

        // Send well above the threshold count to ensure no false positive.
        for ts in 2..=(PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD + 3) {
            let heartbeat = json!({
                "type": "INBOUND_MESSAGE",
                "tabId": 7,
                "payload": json!({
                    "turn_id": 11,
                    "lease_token": "lease-expected",
                    "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room","source":{"trigger_message_id":"trigger-1","client_request_id":"req-1","model_slug":"gpt-5-4-auto-thinking","message":"**ChatGPT** is taking a look"}}}}]"#,
                    "ts": ts,
                })
                .to_string(),
            })
            .to_string();
            handle_inbound(&heartbeat, &state).await;
        }

        let no_fail = tokio::time::timeout(std::time::Duration::from_millis(100), early_fail_rx).await;
        assert!(
            no_fail.is_err(),
            "responding heartbeats are liveness evidence and must not trigger early_fail"
        );
    }

    #[tokio::test]
    async fn changing_responding_heartbeat_status_resets_pre_turn_stall_counter() {
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
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-message-add","payload":{"message":{"id":"room~room~CalpicoMessage~trigger-1","role":"user","content":{"text":"prompt"}}}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&user_echo, &state).await;

        for ts in 2..=5 {
            let heartbeat = json!({
                "type": "INBOUND_MESSAGE",
                "tabId": 7,
                "payload": json!({
                    "turn_id": 11,
                    "lease_token": "lease-expected",
                    "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room","source":{"trigger_message_id":"trigger-1","client_request_id":"req-1","model_slug":"gpt-5-4-auto-thinking","message":"**ChatGPT** is taking a look"}}}}]"#,
                    "ts": ts,
                })
                .to_string(),
            })
            .to_string();
            handle_inbound(&heartbeat, &state).await;
        }

        for ts in 6..=9 {
            let heartbeat = json!({
                "type": "INBOUND_MESSAGE",
                "tabId": 7,
                "payload": json!({
                    "turn_id": 11,
                    "lease_token": "lease-expected",
                    "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room","source":{"trigger_message_id":"trigger-1","client_request_id":"req-1","model_slug":"gpt-5-4-auto-thinking","message":"**ChatGPT** is thinking"}}}}]"#,
                    "ts": ts,
                })
                .to_string(),
            })
            .to_string();
            handle_inbound(&heartbeat, &state).await;
        }

        let no_fail = tokio::time::timeout(std::time::Duration::from_millis(100), early_fail_rx).await;
        assert!(
            no_fail.is_err(),
            "progressive heartbeat status changes should not deterministically early-fail"
        );
    }

    // Even when the counter is preloaded to the threshold, a responding heartbeat
    // must not fire early_fail — liveness evidence always wins.
    #[tokio::test]
    async fn responding_heartbeat_does_not_fail_when_counter_is_already_at_threshold() {
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
            st.user_message_seen.insert((7, 11), 1);
            st.post_user_message_heartbeat.insert(
                (7, 11),
                PRE_TURN_COMPLETE_HEARTBEAT_STALL_THRESHOLD,
            );
            st.assemblers
                .insert(7, FrameAssembler::new(SiteType::ChatGptGroup));
        }

        let heartbeat = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room"}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&heartbeat, &state).await;

        let no_fail = tokio::time::timeout(std::time::Duration::from_millis(100), early_fail_rx).await;
        assert!(
            no_fail.is_err(),
            "responding heartbeat must not trigger early_fail even when counter is at threshold"
        );
    }

    #[tokio::test]
    async fn first_responding_heartbeat_after_user_echo_gets_grace() {
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
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-message-add","payload":{"message":{"id":"room~room~CalpicoMessage~trigger-1","role":"user","content":{"text":"prompt"}}}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&user_echo, &state).await;

        let heartbeat = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{"type":"message","topic_id":"calpico-chatgpt","payload":{"type":"calpico-is-responding-heartbeat","payload":{"room_id":"room","source":{"trigger_message_id":"trigger-1","message":"**ChatGPT** is taking a look"}}}}]"#,
                "ts": 2,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&heartbeat, &state).await;

        let no_fail = tokio::time::timeout(std::time::Duration::from_millis(100), early_fail_rx).await;
        assert!(
            no_fail.is_err(),
            "the first responding heartbeat after user echo should not deterministically early-fail"
        );

        let st = state.lock().await;
        assert_eq!(st.post_user_message_heartbeat.get(&(7, 11)).copied(), Some(0));
    }

    #[tokio::test]
    async fn quarantined_tab_inbound_is_ignored_until_tab_ready_reannounces() {
        let state = Arc::new(Mutex::new(State::new()));
        {
            let mut st = state.lock().await;
            st.quarantined_tabs.insert(7);
        }

        let inbound = json!({
            "type": "INBOUND_MESSAGE",
            "tabId": 7,
            "payload": json!({
                "turn_id": 11,
                "lease_token": "lease-expected",
                "chunk": r#"[{\"type\":\"message\",\"topic_id\":\"calpico-chatgpt\",\"payload\":{\"type\":\"calpico-message-add\",\"payload\":{\"message\":{\"id\":\"room~room~CalpicoMessage~trigger-1\",\"role\":\"user\",\"content\":{\"text\":\"prompt\"}}}}}]"#,
                "ts": 1,
            })
            .to_string(),
        })
        .to_string();
        handle_inbound(&inbound, &state).await;

        let st = state.lock().await;
        assert!(st.pending_turn_id.is_empty());
        assert!(st.quarantined_tabs.contains(&7));
        drop(st);

        let tab_ready = json!({
            "type": "TAB_READY",
            "tabId": 7,
            "url": "https://chatgpt.com/",
        })
        .to_string();
        handle_inbound(&tab_ready, &state).await;

        let st = state.lock().await;
        assert!(!st.quarantined_tabs.contains(&7));
        assert!(st
            .preopened
            .get("https://chatgpt.com/")
            .map(|queue| queue.contains(&7))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn retire_stateful_tab_quarantines_until_ready() {
        let state = Arc::new(Mutex::new(State::new()));
        {
            let mut st = state.lock().await;
            st.tab_urls.insert(7, "https://chatgpt.com/".to_string());
            st.endpoint_tabs.insert("planner_chatgpt".to_string(), 7);
            st.tab_owners.insert(7, "planner_chatgpt".to_string());
            retire_tab_locked(&mut st, "planner_chatgpt", 7, true);
            assert!(st.quarantined_tabs.contains(&7));
            assert_eq!(st.endpoint_tabs.get("planner_chatgpt").copied(), Some(7));
            assert_eq!(st.tab_owners.get(&7).map(String::as_str), Some("planner_chatgpt"));
        }

        let tab_ready = json!({
            "type": "TAB_READY",
            "tabId": 7,
            "url": "https://chatgpt.com/",
        })
        .to_string();
        handle_inbound(&tab_ready, &state).await;

        let st = state.lock().await;
        assert!(!st.quarantined_tabs.contains(&7));
        assert_eq!(st.endpoint_tabs.get("planner_chatgpt").copied(), Some(7));
        assert_eq!(st.tab_owners.get(&7).map(String::as_str), Some("planner_chatgpt"));
        assert!(!st
            .preopened
            .get("https://chatgpt.com/")
            .map(|queue| queue.contains(&7))
            .unwrap_or(false));
    }
}

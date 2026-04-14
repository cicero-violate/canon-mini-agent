/// Chromium backend: acts as a WebSocket server that the Canon Chrome extension
/// connects to.  Prompts are injected into a ChatGPT tab via the extension
/// relay; SSE frames come back as INBOUND_MESSAGE and are assembled by the
/// same `parsers::FrameAssembler` used in canon-llm-runtime.
///
/// Protocol (extension → Rust):
///   TAB_READY      { tabId, url, reqId? }  — a ChatGPT tab is ready
///   TAB_OPENED     { tabId, url, reqId? }  — newly opened tab navigated
///   TAB_CLOSED     { tabId }
///   SUBMIT_ACK     { tabId, turnId }        — prompt was submitted
///   INBOUND_MESSAGE{ tabId, payload }       — SSE chunk from ChatGPT
///   PING                                    — keepalive, ignored
///
/// Protocol (Rust → extension):
///   OPEN_TAB       { url, reqId }           — open a new tab
///   CLAIM_TAB      { tabId, url }           — assert ownership after sw restart
///   TURN           { tabId, text, turnId }  — inject prompt
use crate::llm_runtime::backend::LlmBackend;
use crate::llm_runtime::parsers::{FrameAssembler, SiteType};
use crate::llm_runtime::types::LlmResponse;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const FRAMES_DIR: &str = "./frames";

fn append_jsonl(filename: &str, value: &Value) {
    let path = format!("{FRAMES_DIR}/{filename}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if let Ok(line) = serde_json::to_string(value) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn init_frames_dir() {
    let _ = std::fs::create_dir_all(FRAMES_DIR);
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

    /// (tabId, turnId) → oneshot that fires when SUBMIT_ACK arrives.
    pending_ack: HashMap<(u32, u64), oneshot::Sender<()>>,

    /// (tabId, turnId) → oneshot that fires with the assembled response.
    pending_resp: HashMap<(u32, u64), oneshot::Sender<String>>,

    /// reqId → oneshot that fires with the tabId when TAB_READY (with reqId) arrives.
    pending_open: HashMap<u64, oneshot::Sender<u32>>,

    /// tabId → URL (last known).
    tab_urls: HashMap<u32, String>,

    /// URL → queue of pre-opened tabIds (TAB_READY with no reqId).
    preopened: HashMap<String, VecDeque<u32>>,

    /// TURN frames queued while the extension socket is down.
    replay_queue: Vec<Value>,

    /// Monotonic counter for frame log entries.
    frame_counter: u64,
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
            pending_ack: HashMap::new(),
            pending_resp: HashMap::new(),
            pending_open: HashMap::new(),
            tab_urls: HashMap::new(),
            preopened: HashMap::new(),
            replay_queue: Vec::new(),
            frame_counter: 0,
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
    next_req_id: Arc<AtomicU64>,
    port: u16,
}

impl ChromiumBackend {
    /// Spawn the WebSocket server on `port` and return the backend handle.
    pub fn spawn(port: u16) -> Self {
        let state = Arc::new(Mutex::new(State::new()));
        let backend = ChromiumBackend {
            state: state.clone(),
            next_turn_id: Arc::new(AtomicU64::new(1)),
            next_req_id: Arc::new(AtomicU64::new(1)),
            port,
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
    async fn acquire_tab(&self, urls: &[String], timeout_secs: u64) -> Result<u32> {
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

        // Give TAB_READY messages a moment to arrive after connection.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Check pool first.
        if let Some(tab_id) = self.pop_any_tab().await {
            return Ok(tab_id);
        }

        // No tab available — open one at the first URL.
        let url = urls.first().map(String::as_str).unwrap_or("https://chatgpt.com/");
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
            .max(10);
        eprintln!("[chromium] no tab available, opening {url}");
        self.open_tab(url, remaining).await
    }

    /// Send a TURN to the extension and wait for the full assembled response.
    async fn do_send(&self, tab_id: u32, url: &str, prompt: &str, timeout_secs: u64) -> Result<LlmResponse> {
        let turn_id = self.next_turn_id.fetch_add(1, Ordering::SeqCst);
        let (ack_tx, ack_rx) = oneshot::channel::<()>();
        let (resp_tx, resp_rx) = oneshot::channel::<String>();

        {
            let mut st = self.state.lock().await;
            st.pending_ack.insert((tab_id, turn_id), ack_tx);
            st.pending_resp.insert((tab_id, turn_id), resp_tx);
            st.pending_turn_id.insert(tab_id, turn_id);

            let site = SiteType::from_url(url);
            st.assemblers
                .entry(tab_id)
                .and_modify(|a| { a.set_site(site); a.reset(); })
                .or_insert_with(|| FrameAssembler::new(site));

            let frame = json!({ "type": "TURN", "tabId": tab_id, "text": prompt, "turnId": turn_id });
            if !st.send_msg(frame.clone()) {
                st.replay_queue.push(frame);
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        // Wait for SUBMIT_ACK (15s cap).
        let ack_deadline = deadline.min(
            std::time::Instant::now() + std::time::Duration::from_secs(15),
        );
        let ack_remaining = ack_deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(ack_remaining, ack_rx).await {
            Ok(Ok(())) => {}
            _ => {
                let mut st = self.state.lock().await;
                st.pending_ack.remove(&(tab_id, turn_id));
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                // Return tab to pool so the next request can reuse it.
                let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
                st.preopened.entry(tab_url).or_default().push_back(tab_id);
                anyhow::bail!("chromium: timeout waiting for SUBMIT_ACK (tab={tab_id} turn={turn_id})");
            }
        }

        // Wait for full response.
        let resp_remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(resp_remaining, resp_rx).await {
            Ok(Ok(raw)) => {
                let mut st = self.state.lock().await;
                st.pending_turn_id.remove(&tab_id);
                let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
                st.preopened.entry(tab_url).or_default().push_back(tab_id);
                Ok(LlmResponse { raw, tab_id: Some(tab_id), turn_id: Some(turn_id) })
            }
            _ => {
                let mut st = self.state.lock().await;
                st.pending_resp.remove(&(tab_id, turn_id));
                st.pending_turn_id.remove(&tab_id);
                let tab_url = st.tab_urls.get(&tab_id).cloned().unwrap_or_default();
                st.preopened.entry(tab_url).or_default().push_back(tab_id);
                anyhow::bail!("chromium: timeout waiting for response (tab={tab_id} turn={turn_id})");
            }
        }
    }
}

#[async_trait]
impl LlmBackend for ChromiumBackend {
    async fn send(
        &self,
        _endpoint_id: &str,
        urls: &[String],
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

        if submit_only {
            let turn_id = self.next_turn_id.fetch_add(1, Ordering::SeqCst);
            // For submit-only, try to use an existing tab but don't block if none.
            let tab_id = self.pop_any_tab().await.unwrap_or(0) as u32;
            {
                let mut st = self.state.lock().await;
                let frame = json!({ "type": "TURN", "tabId": tab_id, "text": full_prompt, "turnId": turn_id });
                if !st.send_msg(frame.clone()) {
                    st.replay_queue.push(frame);
                }
            }
            return Ok(LlmResponse {
                raw: format!(r#"{{"submit_ack":true,"tab_id":{tab_id},"turn_id":{turn_id}}}"#),
                tab_id: Some(tab_id),
                turn_id: Some(turn_id),
            });
        }

        let tab_id = self.acquire_tab(urls, timeout).await?;
        let url = {
            let st = self.state.lock().await;
            st.tab_urls.get(&tab_id).cloned().unwrap_or_else(|| {
                urls.first().cloned().unwrap_or_default()
            })
        };

        self.do_send(tab_id, &url, &full_prompt, timeout).await
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
        // Clear frame logs so each run starts fresh.
        for name in &["all.jsonl", "inbound.jsonl", "assembled.jsonl"] {
            let _ = std::fs::remove_file(format!("{FRAMES_DIR}/{name}"));
        }
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
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mut st = state.lock().await;
            st.tab_urls.insert(tab_id, url.clone());
            let site = SiteType::from_url(&url);
            st.assemblers.entry(tab_id).or_insert_with(|| FrameAssembler::new(site));
        }

        // A tab is ready (content script loaded).
        "TAB_READY" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let original_url = msg.get("originalUrl").and_then(|v| v.as_str()).map(str::to_string);
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
            for q in st.preopened.values_mut() {
                q.retain(|id| *id != tab_id);
            }
            st.pending_turn_id.remove(&tab_id);
            st.pending_ack.retain(|(tid, _), _| *tid != tab_id);
            st.pending_resp.retain(|(tid, _), _| *tid != tab_id);
        }

        "SUBMIT_ACK" => {
            let tab_id = match msg.get("tabId").and_then(|v| v.as_u64()) {
                Some(id) => id as u32,
                None => return,
            };
            let turn_id = match msg.get("turnId").and_then(|v| v.as_u64()) {
                Some(id) => id,
                None => return,
            };
            let mut st = state.lock().await;
            if let Some(tx) = st.pending_ack.remove(&(tab_id, turn_id)) {
                let _ = tx.send(());
            }
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
            if payload_raw.trim_start().starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(&payload_raw) {
                    if let Some(c) = v.get("chunk").and_then(|c| c.as_str()) {
                        chunk = c.to_string();
                    }
                    inbound_turn_id = v.get("turn_id").and_then(|t| t.as_u64());
                }
            }

            let mut st = state.lock().await;
            let expected_turn_id = st.pending_turn_id.get(&tab_id).copied();

            // Log every inbound chunk with full context.
            st.frame_counter += 1;
            append_jsonl("inbound.jsonl", &json!({
                "frame_counter": st.frame_counter,
                "tab_id": tab_id,
                "inbound_turn_id": inbound_turn_id,
                "expected_turn_id": expected_turn_id,
                "chunk": chunk,
                "payload_raw_len": payload_raw.len(),
            }));

            let expected_turn_id = match expected_turn_id {
                Some(id) => id,
                None => return,
            };
            // Drop frames whose turn_id doesn't match what we're waiting for.
            if let Some(itid) = inbound_turn_id {
                if itid != expected_turn_id {
                    return;
                }
            }
            let turn_id = expected_turn_id;

            let assembled = if let Some(asm) = st.assemblers.get_mut(&tab_id) {
                asm.push(&chunk)
            } else {
                None
            };

            if let Some(text) = assembled {
                append_jsonl("assembled.jsonl", &json!({
                    "tab_id": tab_id,
                    "turn_id": turn_id,
                    "text_len": text.len(),
                    "text_preview": &text[..text.len().min(200)],
                }));
                if let Some(tx) = st.pending_resp.remove(&(tab_id, turn_id)) {
                    let _ = tx.send(text);
                }
            }
        }

        _ => {}
    }
}

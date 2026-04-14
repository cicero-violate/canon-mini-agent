use std::collections::HashMap;
use std::sync::Arc;

pub struct TabSlotTable {
    pub owner: HashMap<String, u32>,
    pub meta: HashMap<u32, TabSlotMeta>,
    pub endpoint_backoff_ms: HashMap<String, u64>,
    pub endpoint_cooldown_until_ms: HashMap<String, u128>,
}

impl TabSlotTable {
    pub fn new() -> Self {
        Self {
            owner: HashMap::new(),
            meta: HashMap::new(),
            endpoint_backoff_ms: HashMap::new(),
            endpoint_cooldown_until_ms: HashMap::new(),
        }
    }
}

impl Default for TabSlotTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TabSlotMeta {
    pub last_sent_ms: Option<u128>,
    pub last_response_ms: Option<u128>,
    pub in_flight: bool,
    pub cooldown_until_ms: Option<u128>,
}

const ADAPTIVE_MIN_COOLDOWN_MS: u64 = 1_000;
const ADAPTIVE_MAX_COOLDOWN_MS: u64 = 8_000;
const ADAPTIVE_SUCCESS_DECAY_MS: u64 = 250;
const ADAPTIVE_FAILURE_MULTIPLIER: u64 = 2;

pub type TabManagerHandle = Arc<tokio::sync::Mutex<TabSlotTable>>;

pub async fn tab_manager_get_owner_tab(endpoint_id: &str, tabs: &TabManagerHandle) -> Option<u32> {
    loop {
        let wait_ms = {
            let mut tabs = tabs.lock().await;
            let id = tabs.owner.get(endpoint_id).copied()?;
            let meta = tabs.meta.entry(id).or_default();
            let now = tab_manager_now_ms();
            let cooldown_until = meta.cooldown_until_ms.unwrap_or(0);
            if cooldown_until > now {
                Some((id, cooldown_until.saturating_sub(now) as u64))
            } else {
                meta.in_flight = true;
                return Some(id);
            }
        };
        let (_id, delay_ms) = wait_ms?;
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
}

pub async fn tab_manager_set_tab_id(
    endpoint_id: &str,
    id: u32,
    tabs: &TabManagerHandle,
    _max_tabs: usize,
) {
    let mut tabs = tabs.lock().await;
    tabs.owner.insert(endpoint_id.to_string(), id);
    tabs.meta.entry(id).or_default();
}

pub async fn tab_manager_mark_tab_sent(tabs: &TabManagerHandle, id: u32) {
    let mut tabs = tabs.lock().await;
    let meta = tabs.meta.entry(id).or_default();
    meta.last_sent_ms = Some(tab_manager_now_ms());
}

pub async fn tab_manager_mark_tab_response(tabs: &TabManagerHandle, id: u32) {
    let mut tabs = tabs.lock().await;
    let meta = tabs.meta.entry(id).or_default();
    meta.last_response_ms = Some(tab_manager_now_ms());
}

pub async fn tab_manager_mark_tab_in_flight(tabs: &TabManagerHandle, id: u32, in_flight: bool) {
    let mut tabs = tabs.lock().await;
    let meta = tabs.meta.entry(id).or_default();
    meta.in_flight = in_flight;
}

pub async fn tab_manager_note_success(tabs: &TabManagerHandle, endpoint_id: &str, id: u32) -> u64 {
    let mut tabs = tabs.lock().await;
    let now = tab_manager_now_ms();
    let current = tabs
        .endpoint_backoff_ms
        .get(endpoint_id)
        .copied()
        .unwrap_or(ADAPTIVE_MIN_COOLDOWN_MS);
    let next = current
        .saturating_sub(ADAPTIVE_SUCCESS_DECAY_MS)
        .max(ADAPTIVE_MIN_COOLDOWN_MS);
    tabs.endpoint_backoff_ms
        .insert(endpoint_id.to_string(), next);
    tabs.endpoint_cooldown_until_ms
        .insert(endpoint_id.to_string(), now.saturating_add(next as u128));
    let meta = tabs.meta.entry(id).or_default();
    meta.cooldown_until_ms = Some(now.saturating_add(next as u128));
    next
}

pub async fn tab_manager_apply_rate_limit_penalty(
    tabs: &TabManagerHandle,
    endpoint_id: &str,
) -> u64 {
    let mut tabs = tabs.lock().await;
    let now = tab_manager_now_ms();
    let current = tabs
        .endpoint_backoff_ms
        .get(endpoint_id)
        .copied()
        .unwrap_or(ADAPTIVE_MIN_COOLDOWN_MS);
    let grown = current
        .max(ADAPTIVE_MIN_COOLDOWN_MS)
        .saturating_mul(ADAPTIVE_FAILURE_MULTIPLIER);
    let next = grown.clamp(ADAPTIVE_MIN_COOLDOWN_MS, ADAPTIVE_MAX_COOLDOWN_MS);
    tabs.endpoint_backoff_ms
        .insert(endpoint_id.to_string(), next);
    tabs.endpoint_cooldown_until_ms
        .insert(endpoint_id.to_string(), now.saturating_add(next as u128));
    next
}

pub async fn tab_manager_drop_tab(tabs: &TabManagerHandle, endpoint_id: &str, id: u32) {
    let mut tabs = tabs.lock().await;
    if let Some(current) = tabs.owner.get(endpoint_id).copied() {
        if current == id {
            tabs.owner.remove(endpoint_id);
        }
    }
    tabs.meta.remove(&id);
}

pub async fn tab_manager_summarize_tab_state(
    endpoint_id: &str,
    tabs: &TabManagerHandle,
) -> Option<String> {
    let tabs = tabs.lock().await;
    let id = tabs.owner.get(endpoint_id).copied()?;
    let meta = tabs.meta.get(&id);
    let in_flight = meta.map(|m| m.in_flight).unwrap_or(false);
    let last_resp = meta.and_then(|m| m.last_response_ms).unwrap_or(0);
    let cooldown = meta.and_then(|m| m.cooldown_until_ms).unwrap_or(0);
    let endpoint_backoff = tabs
        .endpoint_backoff_ms
        .get(endpoint_id)
        .copied()
        .unwrap_or(ADAPTIVE_MIN_COOLDOWN_MS);
    let endpoint_cooldown = tabs
        .endpoint_cooldown_until_ms
        .get(endpoint_id)
        .copied()
        .unwrap_or(0);
    Some(format!(
        "tab={} in_flight={} last_resp_ms={} cooldown_until_ms={} endpoint_backoff_ms={} endpoint_cooldown_until_ms={}",
        id, in_flight, last_resp, cooldown, endpoint_backoff, endpoint_cooldown
    ))
}

pub fn tab_manager_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

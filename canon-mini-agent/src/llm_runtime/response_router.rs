use once_cell::sync::Lazy;
use std::collections::HashMap;
use tokio::sync::Mutex;
static ROUTES: Lazy<Mutex<HashMap<u64, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
pub async fn response_router_register(req_id: u64, node_id: &str) {
    let mut routes = ROUTES.lock().await;
    routes.insert(req_id, node_id.to_string());
}
pub async fn response_router_resolve(req_id: u64) -> Option<String> {
    let mut routes = ROUTES.lock().await;
    routes.remove(&req_id)
}

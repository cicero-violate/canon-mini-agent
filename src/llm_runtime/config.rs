/// Minimal LlmEndpoint — the only fields canon-mini-agent uses.
#[derive(Debug, Clone)]
pub struct LlmEndpoint {
    pub id: String,
    pub url: Vec<String>,
    pub role_markdown: String,
    pub role: Option<String>,
    pub stateful: bool,
    pub max_tabs: usize,
}

impl LlmEndpoint {
    pub fn pick_url(&self, index: usize) -> &str {
        match self.url.len() {
            0 => "",
            len => &self.url[index % len],
        }
    }
}

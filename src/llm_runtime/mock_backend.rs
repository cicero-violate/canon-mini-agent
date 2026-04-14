use crate::llm_runtime::backend::LlmBackend;
use crate::llm_runtime::types::LlmResponse;
use anyhow::Result;
use async_trait::async_trait;

/// Test mock that returns a fixed response string for every call.
pub struct MockBackend {
    response: String,
}

impl MockBackend {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
        }
    }

    pub fn empty() -> Self {
        Self::new("")
    }
}

#[async_trait]
impl LlmBackend for MockBackend {
    async fn send(
        &self,
        _endpoint_id: &str,
        _urls: &[String],
        _prompt: &str,
        _system_schema: &str,
        _submit_only: bool,
        _timeout_secs: Option<u64>,
    ) -> Result<LlmResponse> {
        Ok(LlmResponse {
            raw: self.response.clone(),
            tab_id: Some(1),
            turn_id: Some(1),
        })
    }
}

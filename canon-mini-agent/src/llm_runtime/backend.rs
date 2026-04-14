use crate::llm_runtime::types::LlmResponse;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn send(
        &self,
        endpoint_id: &str,
        urls: &[String],
        stateful: bool,
        prompt: &str,
        system_schema: &str,
        submit_only: bool,
        timeout_secs: Option<u64>,
    ) -> Result<LlmResponse>;
}

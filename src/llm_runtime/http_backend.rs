use crate::llm_runtime::backend::LlmBackend;
use crate::llm_runtime::types::LlmResponse;
use anyhow::{anyhow, Result};
use async_trait::async_trait;

pub struct HttpBackend {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl HttpBackend {
    /// Intent: transport_effect
    /// Resource: error
    /// Inputs: ()
    /// Outputs: llm_runtime::http_backend::HttpBackend
    /// Effects: uses_network
    /// Forbidden: error
    /// Invariants: error
    /// Failure: error
    /// Provenance: rustc:facts + rustc:docstring
    pub fn from_env() -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let model =
            std::env::var("CANON_LLM_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        }
    }
}

/// Intent: transport_effect
#[async_trait]
impl LlmBackend for HttpBackend {
    async fn send(
        &self,
        _endpoint_id: &str,
        _urls: &[String],
        _stateful: bool,
        prompt: &str,
        system_schema: &str,
        submit_only: bool,
        timeout_secs: Option<u64>,
    ) -> Result<LlmResponse> {
        if submit_only {
            return Ok(LlmResponse {
                raw: r#"{"submit_ack":true}"#.to_string(),
                tab_id: Some(1),
                turn_id: Some(1),
            });
        }

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 8192,
            "system": system_schema,
            "messages": [{"role": "user", "content": prompt}]
        });

        let mut req = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        if let Some(secs) = timeout_secs {
            req = req.timeout(std::time::Duration::from_secs(secs));
        }

        let resp = req.send().await?.error_for_status()?;
        let value: serde_json::Value = resp.json().await?;

        let raw = value
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("unexpected Anthropic response shape: {}", value))?
            .to_string();

        Ok(LlmResponse {
            raw,
            tab_id: None,
            turn_id: None,
        })
    }
}

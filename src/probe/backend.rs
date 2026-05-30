//! LLM backend for the probe agent — async, using reqwest.
//!
//! Talks to any OpenAI-compatible chat completions API.
//! Uses a concrete `OpenAiBackend` struct (no trait) to avoid extra dependencies.

use crate::probe::types::{Message, LlmResponse, parse_llm_response};
use serde_json::Value;

/// OpenAI-compatible backend using reqwest.
pub struct OpenAiBackend {
    client: reqwest::Client,
    api_base: String,
    model: String,
    api_key: String,
}

impl OpenAiBackend {
    pub fn new(api_base: &str, model: &str, api_key: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to create HTTP client for probe");
        Self {
            client,
            api_base: api_base.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
        }
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
    ) -> Result<LlmResponse, String> {
        let url = format!("{}/chat/completions", self.api_base);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });

        if let Value::Array(arr) = tools {
            if !arr.is_empty() {
                body["tools"] = tools.clone();
                body["tool_choice"] = serde_json::json!("auto");
            }
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("LLM request failed: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("LLM API error {}: {}", status, text));
        }

        let text = resp.text().await.map_err(|e| format!("reading response: {}", e))?;
        parse_llm_response(&text)
    }
}

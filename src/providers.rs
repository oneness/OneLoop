use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use crate::agent::messages::ToolCall;

/// Request sent to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::agent::messages::Message>,
    pub tools: Vec<crate::tools::ToolDefinition>,
    /// Override the provider's configured model for this request only.
    pub model_override: Option<String>,
}

/// Response from a provider.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> String;
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;
}

pub mod anthropic;
pub mod openai;
pub mod openrouter;
pub mod registry;

// Re-export key types for convenience.
pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
pub use openrouter::OpenRouterProvider;
pub use registry::ProviderRegistry;

/// Send a prepared provider request, check the status, and return the raw
/// body text. Non-2xx responses become errors carrying the provider's own
/// error message.
async fn send_and_read(request: reqwest::RequestBuilder, provider: &str) -> Result<String> {
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to send request to {provider}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read {provider} response body"))?;
    if !status.is_success() {
        bail!(
            "{provider} request failed ({status}): {}",
            extract_error_message(&text)
        );
    }
    Ok(text)
}

/// Extract a concise error message from a provider JSON error response.
/// Falls back to truncating the raw text at 200 characters.
fn extract_error_message(raw: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        // Try common shapes: {"error": {"message": "..."}} or {"error": "string"}
        if let Some(error) = val.get("error") {
            if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
            if let Some(msg) = error.as_str() {
                return msg.to_string();
            }
        }
    }
    // Last resort: truncate the raw text.
    let truncated = crate::output::truncate_at_char_boundary(raw, 200);
    if truncated.len() < raw.len() {
        format!("{truncated}…")
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::extract_error_message;

    #[test]
    fn error_truncation_handles_multibyte_text() {
        // 199 ASCII bytes, then a two-byte character straddling byte 200 —
        // the old byte-index slice panicked here.
        let raw = format!("{}écurité and more trailing text", "x".repeat(199));
        let msg = extract_error_message(&raw);
        assert!(msg.ends_with('…'));
        assert!(msg.starts_with("xxx"));
    }
}

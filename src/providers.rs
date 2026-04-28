use anyhow::Result;
use async_trait::async_trait;

use crate::agent::messages::ToolCall;

/// Request sent to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::agent::messages::Message>,
    pub tools: Vec<crate::tools::ToolDefinition>,
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
pub mod registry;
pub mod zai;

// Re-export key types for convenience.
pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
pub use registry::ProviderRegistry;
pub use zai::ZaiProvider;

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
        // Fallback: first string value we find
        if let Some(msg) = val.pointer("/error/message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    // Last resort: truncate raw text
    let limit = 200;
    if raw.len() > limit {
        format!("{}…", &raw[..limit])
    } else {
        raw.to_string()
    }
}

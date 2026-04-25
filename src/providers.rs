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

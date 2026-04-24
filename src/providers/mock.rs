use anyhow::Result;
use async_trait::async_trait;

use crate::agent::messages::{Message, ToolCall};

use super::{Provider, ProviderRequest, ProviderResponse};

pub struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn model(&self) -> String {
        "mock".to_string()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        if let Some(Message::ToolResult(tool_result)) = request.messages.last() {
            return Ok(ProviderResponse {
                content: format!(
                    "[mock provider] Completed tool {}. Result:\n{}",
                    tool_result.tool_name, tool_result.content
                ),
                tool_calls: Vec::new(),
            });
        }

        let last_user = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => Some(user.content.as_str()),
                _ => None,
            })
            .unwrap_or("(no user message)");

        if let Some(path) = last_user.strip_prefix("please read ") {
            return Ok(ProviderResponse {
                content: "I will read that file.".to_string(),
                tool_calls: vec![ToolCall {
                    id: "mock-tool-call-1".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": path.trim() }),
                }],
            });
        }

        if let Some(command) = last_user.strip_prefix("please run ") {
            return Ok(ProviderResponse {
                content: "I will run that command.".to_string(),
                tool_calls: vec![ToolCall {
                    id: "mock-tool-call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({ "command": command.trim() }),
                }],
            });
        }

        let system = request
            .system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| format!("\nSystem prompt loaded ({}) bytes.", text.len()))
            .unwrap_or_default();

        let tools = if request.tools.is_empty() {
            "none".to_string()
        } else {
            request
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };

        Ok(ProviderResponse {
            content: format!(
                "[mock provider] You said: {last_user}\nAvailable tools: {tools}{system}"
            ),
            tool_calls: Vec::new(),
        })
    }
}

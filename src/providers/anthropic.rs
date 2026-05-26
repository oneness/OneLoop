use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};

use super::{Provider, ProviderRequest, ProviderResponse, extract_error_message};

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build Anthropic HTTP client")?;

        let model = std::env::var("ONELOOP_ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-6".to_string());

        Ok(Self {
            client,
            api_key,
            model,
        })
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    tools: Vec<AnthropicToolDefinition>,
}

#[derive(Debug, Serialize)]
struct AnthropicToolDefinition {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicInputBlock>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
enum AnthropicInputBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult,
    #[serde(other)]
    Other,
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let model = request
            .model_override
            .as_deref()
            .unwrap_or(&self.model)
            .to_string();
        let body = AnthropicRequest {
            model,
            max_tokens: 4096,
            system: request.system_prompt,
            messages: to_anthropic_messages(request.messages),
            tools: request
                .tools
                .into_iter()
                .map(|tool| AnthropicToolDefinition {
                    name: tool.name,
                    description: tool.description,
                    input_schema: tool.schema,
                })
                .collect(),
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to send request to Anthropic")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read Anthropic response body")?;

        if !status.is_success() {
            let message = extract_error_message(&text);
            bail!("Anthropic request failed ({status}): {message}");
        }

        let parsed: AnthropicResponse =
            serde_json::from_str(&text).context("failed to parse Anthropic response JSON")?;

        let mut content_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in parsed.content {
            match block {
                AnthropicContentBlock::Text { text } => content_parts.push(text),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
                AnthropicContentBlock::ToolResult | AnthropicContentBlock::Other => {}
            }
        }

        Ok(ProviderResponse {
            content: content_parts.join("\n"),
            tool_calls,
        })
    }
}

fn to_anthropic_messages(messages: Vec<Message>) -> Vec<AnthropicMessage> {
    let mut result: Vec<AnthropicMessage> = Vec::new();
    let mut seen_tool_use_ids = HashSet::new();

    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => push_anthropic_block(
                &mut result,
                "user",
                AnthropicInputBlock::Text { text: user.content },
            ),
            Message::Assistant(assistant) => {
                // Anthropic rejects empty text blocks. Skip if content is empty
                // (e.g. model responded with tool calls only, no text).
                if !assistant.content.trim().is_empty() {
                    push_anthropic_block(
                        &mut result,
                        "assistant",
                        AnthropicInputBlock::Text {
                            text: assistant.content,
                        },
                    );
                }
            }
            Message::ToolCall(tool_call) => {
                seen_tool_use_ids.insert(tool_call.id.clone());
                push_anthropic_block(
                    &mut result,
                    "assistant",
                    AnthropicInputBlock::ToolUse {
                        id: tool_call.id,
                        name: tool_call.name,
                        input: tool_call.arguments,
                    },
                );
            }
            Message::ToolResult(tool_result) => {
                if seen_tool_use_ids.contains(&tool_result.tool_call_id) {
                    push_anthropic_block(
                        &mut result,
                        "user",
                        AnthropicInputBlock::ToolResult {
                            tool_use_id: tool_result.tool_call_id,
                            content: tool_result.content,
                            is_error: tool_result.is_error,
                        },
                    );
                }
            }
        }
    }

    result
}

fn push_anthropic_block(
    messages: &mut Vec<AnthropicMessage>,
    role: &str,
    block: AnthropicInputBlock,
) {
    if let Some(last) = messages.last_mut()
        && last.role == role
    {
        last.content.push(block);
        return;
    }

    messages.push(AnthropicMessage {
        role: role.to_string(),
        content: vec![block],
    });
}

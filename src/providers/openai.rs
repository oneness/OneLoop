use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};

use super::{Provider, ProviderRequest, ProviderResponse, extract_error_message};

pub struct OpenAIProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    reasoning_effort: Option<String>,
}

impl OpenAIProvider {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build OpenAI HTTP client")?;

        // Keep this default aligned with the `ol` wrapper.
        let model =
            std::env::var("ONELOOP_OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.6-sol".to_string());
        let base_url = std::env::var("ONELOOP_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        // "none" omits the reasoning parameter entirely — some models reject it.
        let reasoning_effort = match std::env::var("ONELOOP_OPENAI_REASONING_EFFORT") {
            Ok(effort) if effort == "none" => None,
            Ok(effort) => Some(effort),
            Err(_) => Some("medium".to_string()),
        };

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            reasoning_effort,
        })
    }
}

// ── Request types (Responses API) ──────────────────────────────────

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<Value>,
    tools: Vec<ResponsesToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    effort: String,
}

#[derive(Debug, Serialize)]
struct ResponsesToolDefinition {
    #[serde(rename = "type")]
    r#type: String,
    name: String,
    description: String,
    parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

// ── Response types (Responses API) ─────────────────────────────────

/// Top-level response from `/v1/responses`.
#[derive(Debug, Deserialize)]
struct ResponsesApiResponse {
    output: Vec<ResponsesOutputItem>,
}

/// An item in the `output` array — can be a message, function_call, reasoning, etc.
#[derive(Debug, Deserialize)]
struct ResponsesOutputItem {
    #[serde(rename = "type")]
    item_type: String,

    // Present when type == "message"
    content: Option<Vec<ResponsesContentPart>>,

    // Present when type == "function_call"
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContentPart {
    #[serde(rename = "type")]
    part_type: String,
    text: Option<String>,
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &'static str {
        "openai"
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
        let mut input: Vec<Value> = Vec::new();

        for message in request.messages {
            match message {
                Message::System(text) => {
                    // The session's system prompt travels in the top-level
                    // `instructions` field (set below from the request); any
                    // system messages in the history are sent as
                    // "developer"-role message items.
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": text }]
                    }));
                }
                Message::User(user) => {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": user.content }]
                    }));
                }
                Message::Assistant(assistant) => {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": assistant.content }]
                    }));
                }
                Message::ToolCall(tool_call) => {
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": tool_call.id,
                        "name": tool_call.name,
                        "arguments": tool_call.arguments.to_string()
                    }));
                }
                Message::ToolResult(tool_result) => {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_result.tool_call_id,
                        "output": tool_result.content
                    }));
                }
            }
        }

        let body = ResponsesRequest {
            model,
            instructions: request.system_prompt,
            input,
            tools: request
                .tools
                .into_iter()
                .map(|tool| ResponsesToolDefinition {
                    r#type: "function".to_string(),
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.schema,
                    strict: None,
                })
                .collect(),
            reasoning: self
                .reasoning_effort
                .as_ref()
                .map(|effort| ResponsesReasoning {
                    effort: effort.clone(),
                }),
        };

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header(
                "Authorization",
                format!("Bearer {api_key}", api_key = self.api_key),
            )
            .json(&body)
            .send()
            .await
            .context("failed to send request to OpenAI")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read OpenAI response body")?;

        if !status.is_success() {
            let message = extract_error_message(&text);
            bail!("OpenAI request failed ({status}): {message}");
        }

        let parsed: ResponsesApiResponse =
            serde_json::from_str(&text).context("failed to parse OpenAI response JSON")?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();

        for item in parsed.output {
            match item.item_type.as_str() {
                "message" => {
                    if let Some(parts) = item.content {
                        for part in parts {
                            if part.part_type == "output_text"
                                && let Some(txt) = part.text
                            {
                                content.push_str(&txt);
                            }
                        }
                    }
                }
                "function_call" => {
                    let call_id = item.call_id.unwrap_or_default();
                    let fn_name = item.name.unwrap_or_default();
                    let args_str = item.arguments.unwrap_or_else(|| "{}".to_string());
                    let arguments: Value = serde_json::from_str(&args_str).with_context(|| {
                        format!("failed to parse OpenAI tool arguments: {args_str}")
                    })?;
                    tool_calls.push(ToolCall {
                        id: call_id,
                        name: fn_name,
                        arguments,
                    });
                }
                // Skip reasoning items and any other unknown types.
                _ => {}
            }
        }

        Ok(ProviderResponse {
            content,
            tool_calls,
        })
    }
}

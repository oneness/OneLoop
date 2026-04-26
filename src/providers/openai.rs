use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::ToolCall;

use super::{Provider, ProviderRequest, ProviderResponse};

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

        let model = std::env::var("ONELOOP_OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string());
        let base_url = std::env::var("ONELOOP_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let reasoning_effort = env::var("ONELOOP_OPENAI_REASONING_EFFORT")
            .ok()
            .or_else(|| Some("medium".to_string()));

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            reasoning_effort,
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    tools: Vec<OpenAIToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolDefinition {
    #[serde(rename = "type")]
    r#type: String,
    function: OpenAIFunctionDefinition,
}

#[derive(Debug, Serialize)]
struct OpenAIFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    function: OpenAIToolFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

use std::env;

use crate::agent::messages::Message;

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let mut messages = Vec::new();

        if let Some(system) = request.system_prompt {
            messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: Some(system),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        for message in request.messages {
            match message {
                Message::System(text) => {
                    messages.push(OpenAIMessage {
                        role: "system".to_string(),
                        content: Some(text),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Message::User(user) => {
                    messages.push(OpenAIMessage {
                        role: "user".to_string(),
                        content: Some(user.content),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Message::Assistant(assistant) => {
                    messages.push(OpenAIMessage {
                        role: "assistant".to_string(),
                        content: Some(assistant.content),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Message::ToolCall(tool_call) => {
                    messages.push(OpenAIMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_calls: Some(vec![OpenAIToolCall {
                            id: tool_call.id,
                            r#type: "function".to_string(),
                            function: OpenAIToolFunction {
                                name: tool_call.name,
                                arguments: serde_json::to_string(&tool_call.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            },
                        }]),
                        tool_call_id: None,
                    });
                }
                Message::ToolResult(tool_result) => {
                    messages.push(OpenAIMessage {
                        role: "tool".to_string(),
                        content: Some(tool_result.content),
                        tool_calls: None,
                        tool_call_id: Some(tool_result.tool_call_id),
                    });
                }
            }
        }

        let body = OpenAIRequest {
            model: self.model.clone(),
            messages,
            tools: request
                .tools
                .into_iter()
                .map(|tool| OpenAIToolDefinition {
                    r#type: "function".to_string(),
                    function: OpenAIFunctionDefinition {
                        name: tool.name,
                        description: tool.description,
                        parameters: tool.schema,
                    },
                })
                .collect(),
            reasoning_effort: self.reasoning_effort.clone(),
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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
            bail!("OpenAI request failed ({status}): {text}");
        }

        let parsed: OpenAIResponse =
            serde_json::from_str(&text).context("failed to parse OpenAI response JSON")?;

        let Some(choice) = parsed.choices.into_iter().next() else {
            bail!("OpenAI response contained no choices");
        };

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| {
                let arguments =
                    serde_json::from_str(&tc.function.arguments).with_context(|| {
                        format!(
                            "failed to parse OpenAI tool arguments: {}",
                            tc.function.arguments
                        )
                    })?;
                Ok(ToolCall {
                    id: tc.id,
                    name: tc.function.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ProviderResponse {
            content: choice.message.content.unwrap_or_default(),
            tool_calls,
        })
    }
}

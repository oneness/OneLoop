use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};

use super::{extract_error_message, Provider, ProviderRequest, ProviderResponse};

pub struct ZaiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl ZaiProvider {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("Accept-Language", HeaderValue::from_static("en-US,en"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build Z.AI HTTP client")?;

        let model = std::env::var("ONELOOP_ZAI_MODEL").unwrap_or_else(|_| "glm-5.1".to_string());
        let base_url = std::env::var("ONELOOP_ZAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.z.ai/api/coding/paas/v4".to_string());

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
        })
    }
}

#[derive(Debug, Serialize)]
struct ZaiRequest {
    model: String,
    messages: Vec<ZaiMessage>,
    tools: Vec<ZaiToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ZaiToolDefinition {
    #[serde(rename = "type")]
    r#type: String,
    function: ZaiFunctionDefinition,
}

#[derive(Debug, Serialize)]
struct ZaiFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ZaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ZaiToolCall>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ZaiToolCall {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    function: ZaiToolFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ZaiToolFunction {
    name: String,
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct ZaiResponse {
    choices: Vec<ZaiChoice>,
}

#[derive(Debug, Deserialize)]
struct ZaiChoice {
    message: ZaiChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ZaiChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ZaiToolCall>>,
}

#[async_trait]
impl Provider for ZaiProvider {
    fn name(&self) -> &'static str {
        "zai"
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let body = ZaiRequest {
            model: self.model.clone(),
            messages: to_zai_messages(request.messages),
            tools: request
                .tools
                .into_iter()
                .map(|tool| ZaiToolDefinition {
                    r#type: "function".to_string(),
                    function: ZaiFunctionDefinition {
                        name: tool.name,
                        description: tool.description,
                        parameters: tool.schema,
                    },
                })
                .collect(),
            tool_choice: Some("auto".to_string()),
            stream: false,
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .context("failed to send request to Z.AI")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read Z.AI response body")?;

        if !status.is_success() {
            let message = extract_error_message(&text);
            bail!("Z.AI request failed ({status}): {message}");
        }

        let parsed: ZaiResponse =
            serde_json::from_str(&text).context("failed to parse Z.AI response JSON")?;

        let Some(choice) = parsed.choices.into_iter().next() else {
            bail!("Z.AI response contained no choices");
        };

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tool_call| {
                let arguments = match tool_call.function.arguments {
                    Value::String(text) => serde_json::from_str(&text).with_context(|| {
                        format!("failed to parse Z.AI tool arguments JSON: {text}")
                    }),
                    other => Ok(other),
                }?;
                Ok(ToolCall {
                    id: tool_call.id,
                    name: tool_call.function.name,
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

fn to_zai_messages(messages: Vec<Message>) -> Vec<ZaiMessage> {
    messages
        .into_iter()
        .map(|message| match message {
            Message::System(text) => ZaiMessage {
                role: "system".to_string(),
                content: Some(text),
                tool_call_id: None,
                tool_calls: None,
            },
            Message::User(user) => ZaiMessage {
                role: "user".to_string(),
                content: Some(user.content),
                tool_call_id: None,
                tool_calls: None,
            },
            Message::Assistant(assistant) => ZaiMessage {
                role: "assistant".to_string(),
                content: Some(assistant.content),
                tool_call_id: None,
                tool_calls: None,
            },
            Message::ToolCall(tool_call) => ZaiMessage {
                role: "assistant".to_string(),
                content: None,
                tool_call_id: None,
                tool_calls: Some(vec![ZaiToolCall {
                    id: tool_call.id,
                    r#type: "function".to_string(),
                    function: ZaiToolFunction {
                        name: tool_call.name,
                        arguments: Value::String(
                            serde_json::to_string(&tool_call.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                        ),
                    },
                }]),
            },
            Message::ToolResult(tool_result) => ZaiMessage {
                role: "tool".to_string(),
                content: Some(tool_result.content),
                tool_call_id: Some(tool_result.tool_call_id),
                tool_calls: None,
            },
        })
        .collect()
}

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};

use super::{Provider, ProviderRequest, ProviderResponse, extract_error_message};

pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenRouterProvider {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build OpenRouter HTTP client")?;

        let model = std::env::var("ONELOOP_OPENROUTER_MODEL")
            .unwrap_or_else(|_| "anthropic/claude-sonnet-4-5".to_string());
        let base_url = std::env::var("ONELOOP_OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
        })
    }
}

// ── Request types (Chat Completions) ──────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ChatToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatToolDefinition {
    #[serde(rename = "type")]
    r#type: String,
    function: ChatFunctionDefinition,
}

#[derive(Debug, Serialize)]
struct ChatFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    r#type: String,
    function: ChatToolFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatToolFunction {
    name: String,
    arguments: Value,
}

// ── Response types (Chat Completions) ─────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ChatToolCall>>,
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn name(&self) -> &'static str {
        "openrouter"
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

        // Inject system prompt as the first message if present.
        let mut messages = Vec::new();
        if let Some(system) = request.system_prompt {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(system),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        messages.extend(to_chat_messages(request.messages));

        let tools: Vec<ChatToolDefinition> = request
            .tools
            .into_iter()
            .map(|tool| ChatToolDefinition {
                r#type: "function".to_string(),
                function: ChatFunctionDefinition {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.schema,
                },
            })
            .collect();

        // Only set tool_choice when tools are actually provided — some models
        // reject tool_choice: "auto" when the tools array is empty.
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        };

        let body = ChatRequest {
            model,
            messages,
            tools,
            tool_choice,
        };

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
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
            .context("failed to send request to OpenRouter")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read OpenRouter response body")?;

        if !status.is_success() {
            let message = extract_error_message(&text);
            bail!("OpenRouter request failed ({status}): {message}");
        }

        let parsed: ChatResponse =
            serde_json::from_str(&text).context("failed to parse OpenRouter response JSON")?;

        let Some(choice) = parsed.choices.into_iter().next() else {
            bail!("OpenRouter response contained no choices");
        };

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tool_call| {
                let arguments = match tool_call.function.arguments {
                    Value::String(text) => serde_json::from_str(&text).with_context(|| {
                        format!("failed to parse OpenRouter tool arguments JSON: {text}")
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

fn to_chat_messages(messages: Vec<Message>) -> Vec<ChatMessage> {
    let mut result: Vec<ChatMessage> = Vec::new();

    for message in messages {
        match message {
            Message::System(text) => result.push(ChatMessage {
                role: "system".to_string(),
                content: Some(text),
                tool_call_id: None,
                tool_calls: None,
            }),
            Message::User(user) => result.push(ChatMessage {
                role: "user".to_string(),
                content: Some(user.content),
                tool_call_id: None,
                tool_calls: None,
            }),
            Message::Assistant(assistant) => result.push(ChatMessage {
                role: "assistant".to_string(),
                content: Some(assistant.content),
                tool_call_id: None,
                tool_calls: None,
            }),
            Message::ToolCall(tool_call) => {
                let tc = ChatToolCall {
                    id: tool_call.id,
                    r#type: "function".to_string(),
                    function: ChatToolFunction {
                        name: tool_call.name,
                        arguments: Value::String(tool_call.arguments.to_string()),
                    },
                };
                // Merge into the preceding assistant message so that content
                // and tool_calls always appear in a single frame — Chat
                // Completions APIs reject two consecutive assistant messages.
                if let Some(last) = result.last_mut()
                    && last.role == "assistant"
                {
                    last.tool_calls.get_or_insert_with(Vec::new).push(tc);
                } else {
                    result.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: None,
                        tool_call_id: None,
                        tool_calls: Some(vec![tc]),
                    });
                }
            }
            Message::ToolResult(tool_result) => result.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(tool_result.content),
                tool_call_id: Some(tool_result.tool_call_id),
                tool_calls: None,
            }),
        }
    }

    result
}

use std::env;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::{
    agent::messages::{Message, ToolCall},
    auth::{resolve_anthropic_api_key, resolve_openai_api_key, resolve_zai_api_key},
    tools::ToolDefinition,
};

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

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
                .map(|tool| tool.name)
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

        let model = env::var("ONELOOP_ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());

        Ok(Self {
            client,
            api_key,
            model,
        })
    }
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
        let body = AnthropicRequest {
            model: self.model.clone(),
            max_tokens: 4096,
            system: request.system_prompt,
            messages: to_anthropic_messages(request.messages),
            tools: request
                .tools
                .into_iter()
                .map(|tool| AnthropicToolDefinition {
                    name: tool.name.to_string(),
                    description: tool.description.to_string(),
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
            bail!("Anthropic request failed ({}): {}", status, text);
        }

        let parsed: AnthropicResponse = serde_json::from_str(&text)
            .context("failed to parse Anthropic response JSON")?;

        let mut content_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in parsed.content {
            match block {
                AnthropicContentBlock::Text { text } => content_parts.push(text),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall { id, name, arguments: input });
                }
                AnthropicContentBlock::ToolResult => {}
                AnthropicContentBlock::Other => {}
            }
        }

        Ok(ProviderResponse {
            content: content_parts.join("\n"),
            tool_calls,
        })
    }
}

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

        let model = env::var("ONELOOP_ZAI_MODEL").unwrap_or_else(|_| "glm-5.1".to_string());
        let base_url = env::var("ONELOOP_ZAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.z.ai/api/coding/paas/v4".to_string());

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
        })
    }
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
                        name: tool.name.to_string(),
                        description: tool.description.to_string(),
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
            bail!("Z.AI request failed ({}): {}", status, text);
        }

        let parsed: ZaiResponse = serde_json::from_str(&text)
            .context("failed to parse Z.AI response JSON")?;

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
                    Value::String(text) => serde_json::from_str(&text)
                        .with_context(|| format!("failed to parse Z.AI tool arguments JSON: {}", text)),
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
    ToolUse { id: String, name: String, input: Value },
    #[serde(rename = "tool_result")]
    ToolResult,
    #[serde(other)]
    Other,
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

fn to_anthropic_messages(messages: Vec<Message>) -> Vec<AnthropicMessage> {
    let mut result: Vec<AnthropicMessage> = Vec::new();
    let mut seen_tool_use_ids = std::collections::HashSet::new();

    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => push_anthropic_block(&mut result, "user", AnthropicInputBlock::Text { text: user.content }),
            Message::Assistant(assistant) => push_anthropic_block(&mut result, "assistant", AnthropicInputBlock::Text { text: assistant.content }),
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
                )
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

fn push_anthropic_block(messages: &mut Vec<AnthropicMessage>, role: &str, block: AnthropicInputBlock) {
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

        let model = env::var("ONELOOP_OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string());
        let base_url = env::var("ONELOOP_OPENAI_BASE_URL")
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
                        name: tool.name.to_string(),
                        description: tool.description.to_string(),
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
            bail!("OpenAI request failed ({}): {}", status, text);
        }

        let parsed: OpenAIResponse = serde_json::from_str(&text)
            .context("failed to parse OpenAI response JSON")?;

        let Some(choice) = parsed.choices.into_iter().next() else {
            bail!("OpenAI response contained no choices");
        };

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| {
                let arguments = serde_json::from_str(&tc.function.arguments)
                    .with_context(|| format!("failed to parse OpenAI tool arguments: {}", tc.function.arguments))?;
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

pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_index: usize,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let preferred = env::var("ONELOOP_PROVIDER").ok();

        // Build all available providers
        let mut providers: Vec<Box<dyn Provider>> = Vec::new();

        // Always add mock
        let mock_index = providers.len();
        providers.push(Box::new(MockProvider));

        let mut anthropic_index: Option<usize> = None;
        let mut zai_index: Option<usize> = None;
        let mut openai_index: Option<usize> = None;

        if let Some(key) = resolve_anthropic_api_key() {
            anthropic_index = Some(providers.len());
            providers.push(Box::new(AnthropicProvider::new(key)?));
        }

        if let Some(key) = resolve_zai_api_key() {
            zai_index = Some(providers.len());
            providers.push(Box::new(ZaiProvider::new(key)?));
        }

        if let Some(key) = resolve_openai_api_key() {
            openai_index = Some(providers.len());
            providers.push(Box::new(OpenAIProvider::new(key)?));
        }

        // Determine default
        let default_index = match preferred.as_deref() {
            Some("zai") => zai_index.context("ONELOOP_PROVIDER=zai but no ZAI_API_KEY/auth found")?,
            Some("anthropic") => anthropic_index.context("ONELOOP_PROVIDER=anthropic but no ANTHROPIC_API_KEY/auth found")?,
            Some("openai") => openai_index.context("ONELOOP_PROVIDER=openai but no OPENAI_API_KEY/auth found")?,
            Some("mock") => mock_index,
            Some(other) => bail!("unknown provider: {other}"),
            None => zai_index
                .or(openai_index)
                .or(anthropic_index)
                .unwrap_or(mock_index),
        };

        Ok(Self {
            providers,
            default_index,
        })
    }

    pub fn active_name(&self) -> &'static str {
        self.providers[self.default_index].name()
    }

    pub fn active_model(&self) -> String {
        self.providers[self.default_index].model()
    }

    pub fn available_providers(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn resolve(&self, name: Option<&str>) -> Result<&dyn Provider> {
        let name = name.unwrap_or_else(|| self.providers[self.default_index].name());
        let provider = self
            .providers
            .iter()
            .find(|p| p.name() == name)
            .with_context(|| {
                let available: Vec<&str> = self.providers.iter().map(|p| p.name()).collect();
                format!("unknown provider: {name}. available: {}", available.join(", "))
            })?;
        Ok(provider.as_ref())
    }

    pub async fn complete_with(&self, provider_name: Option<&str>, request: ProviderRequest) -> Result<ProviderResponse> {
        let provider = self.resolve(provider_name)?;
        provider.complete(request).await
    }
}

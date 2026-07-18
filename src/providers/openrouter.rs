use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};

use super::{Provider, ProviderRequest, ProviderResponse, send_and_read};

pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    web_tools: bool,
}

impl OpenRouterProvider {
    pub fn new(api_key: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build OpenRouter HTTP client")?;

        // Keep this default aligned with the `ol` wrapper: a wrapper-less
        // invocation must not silently fall back to a different model.
        let model = std::env::var("ONELOOP_OPENROUTER_MODEL")
            .unwrap_or_else(|_| "deepseek/deepseek-v4-flash".to_string());
        let base_url = std::env::var("ONELOOP_OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        // OpenRouter's server-side web_search/web_fetch tools. Metered per
        // use ($0.005/search, $0.001/fetch), so a kill switch is provided.
        let web_tools = crate::config::env_or("ONELOOP_WEB_TOOLS", true);

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            web_tools,
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

/// Either a function tool (`type: "function"` with a definition) or one of
/// OpenRouter's server-side tools (`type: "openrouter:web_search"` etc.,
/// no function body — OpenRouter executes it itself).
#[derive(Debug, Serialize)]
struct ChatToolDefinition {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<ChatFunctionDefinition>,
}

/// Append OpenRouter's server-side web tools to agentic requests. The model
/// decides when to search or fetch; OpenRouter executes server-side and the
/// results come back inside the assistant message. Plain completion calls
/// (no tools) never get them, so synthesis, compaction, and memory
/// extraction can't trigger paid searches.
fn with_web_tools(mut tools: Vec<ChatToolDefinition>, enabled: bool) -> Vec<ChatToolDefinition> {
    if enabled && !tools.is_empty() {
        for name in ["openrouter:web_search", "openrouter:web_fetch"] {
            tools.push(ChatToolDefinition {
                r#type: name.to_string(),
                function: None,
            });
        }
    }
    tools
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
                function: Some(ChatFunctionDefinition {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.schema,
                }),
            })
            .collect();
        let tools = with_web_tools(tools, self.web_tools);

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
        let text = send_and_read(
            self.client
                .post(url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&body),
            "OpenRouter",
        )
        .await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{AssistantMessage, ToolResultMessage, UserMessage};
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: text.into(),
        })
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: text.into(),
        })
    }

    fn tool_call(id: &str) -> Message {
        Message::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        })
    }

    fn tool_result(id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "bash".into(),
            content: "ok".into(),
            is_error: false,
        })
    }

    #[test]
    fn tool_call_merges_into_preceding_assistant_message() {
        // Chat Completions APIs reject two consecutive assistant messages:
        // text and tool_calls must share one frame.
        let result = to_chat_messages(vec![assistant("thinking"), tool_call("t1")]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "assistant");
        assert_eq!(result[0].content.as_deref(), Some("thinking"));
        assert_eq!(result[0].tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn parallel_tool_calls_share_one_assistant_frame() {
        let result = to_chat_messages(vec![assistant("x"), tool_call("t1"), tool_call("t2")]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn tool_call_without_assistant_text_gets_its_own_frame() {
        let result = to_chat_messages(vec![user("q"), tool_call("t1")]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].role, "assistant");
        assert!(result[1].content.is_none());
    }

    #[test]
    fn tool_result_becomes_tool_role_with_call_id() {
        let result = to_chat_messages(vec![tool_result("t1")]);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(result[0].content.as_deref(), Some("ok"));
    }

    #[test]
    fn tool_call_arguments_are_serialized_as_a_json_string() {
        let result = to_chat_messages(vec![tool_call("t1")]);
        let calls = result[0].tool_calls.as_ref().unwrap();
        assert!(matches!(&calls[0].function.arguments, Value::String(s) if s.contains("ls")));
    }

    fn function_tool() -> ChatToolDefinition {
        ChatToolDefinition {
            r#type: "function".to_string(),
            function: Some(ChatFunctionDefinition {
                name: "read".to_string(),
                description: "Read file contents".to_string(),
                parameters: serde_json::json!({}),
            }),
        }
    }

    #[test]
    fn web_tools_are_appended_to_agentic_requests() {
        let tools = with_web_tools(vec![function_tool()], true);
        let types: Vec<&str> = tools.iter().map(|t| t.r#type.as_str()).collect();
        assert_eq!(
            types,
            vec!["function", "openrouter:web_search", "openrouter:web_fetch"]
        );
    }

    #[test]
    fn plain_completion_requests_get_no_web_tools() {
        assert!(with_web_tools(Vec::new(), true).is_empty());
    }

    #[test]
    fn web_tools_can_be_disabled() {
        let tools = with_web_tools(vec![function_tool()], false);
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn server_tools_serialize_as_bare_type() {
        let tools = with_web_tools(vec![function_tool()], true);
        let json = serde_json::to_value(&tools[1]).unwrap();
        assert_eq!(json, serde_json::json!({ "type": "openrouter:web_search" }));
    }
}

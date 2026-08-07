//! OpenAI Chat Completions — the one protocol OneLoop speaks.
//!
//! A local llama-server and a hosted provider differ only by URL, model id,
//! and whether a key is required, so one module serves both. A second
//! protocol would be a module beside this one and a `match` in
//! `Model::complete`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};
use crate::models::Model;

use super::{ProviderRequest, ProviderResponse, send_and_read};

// ── Request types (Chat Completions) ──────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ChatToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    /// Omitted unless configured, so the hosted default still applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
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

/// OpenRouter executes these itself and returns the results inside the
/// assistant message. Plain completion calls never get them, so synthesis,
/// compaction, and memory extraction cannot trigger paid searches.
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
    /// Chat Completions types this as a JSON-*encoded string*, not an object.
    /// Responses are lenient about which one arrives, so this stays a `Value`
    /// on the way in — but echoing an object back into the conversation is
    /// malformed, and a server that renders history through a chat template
    /// turns that into a prompt the model answers with nothing at all.
    #[serde(serialize_with = "serialize_tool_arguments")]
    arguments: Value,
}

/// Always send `arguments` as an encoded object, whichever shape it arrived in.
fn serialize_tool_arguments<S>(arguments: &Value, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&encode_arguments(arguments))
}

/// The one place the wire shape of `arguments` is decided: a JSON object,
/// encoded as a string.
///
/// The field is typed as a string, but a server that renders history through a
/// chat template needs an object *inside* it — Qwen's template reaches for
/// `arguments|items`, and a string or an array there fails the whole request
/// with a 500. The message stays in the session, so that is every request from
/// then on, not just the one.
///
/// A call whose arguments never parsed (`decode_tool_arguments` keeps the raw
/// text) therefore goes out as `{}`. Nothing is lost by it: the tool result
/// travelling beside the call is what tells the model to send it again, and
/// half a truncated argument list is no more use to the model than none.
fn encode_arguments(arguments: &Value) -> String {
    let object = match arguments {
        Value::Object(_) => Some(arguments.to_string()),
        // Already encoded — pass the model's own bytes through rather than
        // re-encoding them, but only once it is known to be an object.
        Value::String(text) => matches!(serde_json::from_str::<Value>(text), Ok(Value::Object(_)))
            .then(|| text.clone()),
        _ => None,
    };
    object.unwrap_or_else(|| "{}".to_string())
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
    /// Reasoning models served locally put their thinking here and leave
    /// `content` null. Not part of the OpenAI schema; absent from the hosted
    /// providers, hence the default.
    #[serde(default)]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ChatToolCall>>,
}

/// Text to show for an assistant turn.
///
/// A turn with tool calls is expected to have no content — the call is the
/// message. Only a turn with neither is a dead end, and there the reasoning
/// beats "I wasn't able to generate a response".
fn assistant_content(
    content: Option<String>,
    reasoning_content: Option<String>,
    has_tool_calls: bool,
) -> String {
    let content = content.filter(|text| !text.trim().is_empty());
    if has_tool_calls {
        return content.unwrap_or_default();
    }
    content
        .or(reasoning_content.filter(|text| !text.trim().is_empty()))
        .unwrap_or_default()
}

/// Keeps the raw text when it will not parse.
///
/// A truncated call is not a transport failure: retrying arrives at the same
/// place a minute later, and the model is the only party that can fix it —
/// which it can only do if told. The raw text keeps the call intact for the
/// history the API requires, and the error travels alongside it.
fn decode_tool_arguments(arguments: Value) -> (Value, Option<String>) {
    match arguments {
        Value::String(text) => match serde_json::from_str(&text) {
            Ok(value) => (value, None),
            Err(err) => (Value::String(text), Some(err.to_string())),
        },
        other => (other, None),
    }
}

pub async fn complete(model: &Model, request: ProviderRequest) -> Result<ProviderResponse> {
    let wire_id = request
        .model_id_override
        .unwrap_or_else(|| model.id.clone());

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
    let tools = with_web_tools(tools, model.web_tools);

    // Only set tool_choice when tools are actually provided — some models
    // reject tool_choice: "auto" when the tools array is empty.
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_string())
    };

    let body = ChatRequest {
        model: wire_id,
        messages,
        tools,
        tool_choice,
        max_tokens: model.max_tokens,
        temperature: model.temperature,
    };

    let post = model.provider.post("chat/completions").json(&body);
    let text = send_and_read(post, &model.alias).await?;

    let parsed: ChatResponse = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {} response JSON", model.alias))?;

    let Some(choice) = parsed.choices.into_iter().next() else {
        bail!("{} response contained no choices", model.alias);
    };

    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tool_call| {
            let (arguments, parse_error) = decode_tool_arguments(tool_call.function.arguments);
            ToolCall {
                id: tool_call.id,
                name: tool_call.function.name,
                arguments,
                parse_error,
            }
        })
        .collect::<Vec<_>>();

    Ok(ProviderResponse {
        content: assistant_content(
            choice.message.content,
            choice.message.reasoning_content,
            !tool_calls.is_empty(),
        ),
        tool_calls,
    })
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
                        // Whatever shape it was stored in — `encode_arguments`
                        // decides the wire form. Pre-encoding here with
                        // `to_string` would encode a raw-text argument list a
                        // second time and send back a string.
                        arguments: tool_call.arguments,
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
            parse_error: None,
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

    /// What a replayed call looks like on the wire, not in the struct.
    #[test]
    fn a_replayed_tool_call_is_sent_as_an_encoded_object() {
        let result = to_chat_messages(vec![tool_call("t1")]);
        let json = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(
            json["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"command":"ls"}"#)
        );
    }

    /// The regression: generation stopped mid-string, `decode_tool_arguments`
    /// kept the raw text, and replaying it encoded that text a second time. The
    /// server parsed it back to a string, its chat template failed on
    /// `arguments|items`, and every later request in the session failed with it.
    #[test]
    fn arguments_that_never_parsed_are_replayed_as_an_empty_object() {
        let truncated = r##"{"new_text":"# Emacs"##;
        let result = to_chat_messages(vec![Message::ToolCall(ToolCall {
            id: "t1".into(),
            name: "edit".into(),
            arguments: Value::String(truncated.to_string()),
            parse_error: Some("EOF while parsing a string".into()),
        })]);
        let json = serde_json::to_value(&result[0]).unwrap();
        assert_eq!(json["tool_calls"][0]["function"]["arguments"], json!("{}"));
    }

    #[test]
    fn only_an_object_reaches_the_wire() {
        assert_eq!(
            encode_arguments(&json!({"path": "a.rs"})),
            r#"{"path":"a.rs"}"#
        );
        let encoded = Value::String(r#"{"path":"a.rs"}"#.to_string());
        assert_eq!(encode_arguments(&encoded), r#"{"path":"a.rs"}"#);
        // Valid JSON, but not an object — the shape that produced
        // `Unknown (built-in) filter 'items' for type String`.
        assert_eq!(
            encode_arguments(&Value::String("\"just text\"".into())),
            "{}"
        );
        assert_eq!(encode_arguments(&json!(["a", "b"])), "{}");
        assert_eq!(encode_arguments(&Value::Null), "{}");
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

    /// Chat Completions types `arguments` as a JSON-encoded string. Echoing a
    /// previous call back as an object is accepted by the hosted APIs and
    /// renders into malformed history on a server that uses a chat template.
    #[test]
    fn tool_call_arguments_serialize_as_a_json_string() {
        let call = ChatToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: ChatToolFunction {
                name: "read".to_string(),
                arguments: serde_json::json!({ "path": "src/main.rs" }),
            },
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(
            json["function"]["arguments"],
            serde_json::json!(r#"{"path":"src/main.rs"}"#)
        );
    }

    /// A string that already arrived encoded must not be encoded twice.
    #[test]
    fn already_encoded_arguments_are_not_double_encoded() {
        let call = ChatToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: ChatToolFunction {
                name: "read".to_string(),
                arguments: Value::String(r#"{"path":"a.rs"}"#.to_string()),
            },
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(
            json["function"]["arguments"],
            serde_json::json!(r#"{"path":"a.rs"}"#)
        );
    }

    #[test]
    fn content_is_used_when_present() {
        let text = assistant_content(Some("answer".into()), Some("thinking".into()), false);
        assert_eq!(text, "answer");
    }

    #[test]
    fn reasoning_content_is_used_when_content_is_null() {
        let text = assistant_content(None, Some("thinking".into()), false);
        assert_eq!(text, "thinking");
    }

    #[test]
    fn reasoning_content_is_used_when_content_is_blank() {
        let text = assistant_content(Some("   ".into()), Some("thinking".into()), false);
        assert_eq!(text, "thinking");
    }

    /// The call is the message on a tool-calling turn; the thinking behind it
    /// is not something to record as if the model had said it.
    #[test]
    fn reasoning_content_is_not_used_when_the_turn_has_tool_calls() {
        let text = assistant_content(None, Some("thinking".into()), true);
        assert_eq!(text, "");
    }

    #[test]
    fn empty_when_neither_is_present() {
        assert_eq!(assistant_content(None, None, false), "");
    }

    #[test]
    fn blank_reasoning_does_not_stand_in_for_an_answer() {
        assert_eq!(assistant_content(None, Some("  \n ".into()), false), "");
    }

    /// The field is absent from every hosted provider's responses.
    #[test]
    fn missing_reasoning_content_field_deserializes() {
        let json = serde_json::json!({ "content": "hi" });
        let message: ChatChoiceMessage = serde_json::from_value(json).unwrap();
        assert_eq!(message.content.as_deref(), Some("hi"));
        assert!(message.reasoning_content.is_none());
    }

    #[test]
    fn max_tokens_is_omitted_when_unset() {
        let body = ChatRequest {
            model: "m".to_string(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: None,
            temperature: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn max_tokens_is_sent_when_set() {
        let body = ChatRequest {
            model: "m".to_string(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: Some(4096),
            temperature: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["max_tokens"], serde_json::json!(4096));
    }

    #[test]
    fn decode_tool_arguments_parses_a_json_string() {
        let (arguments, error) =
            decode_tool_arguments(Value::String(r#"{"command":"ls"}"#.to_string()));
        assert_eq!(arguments, json!({"command": "ls"}));
        assert!(error.is_none());
    }

    #[test]
    fn decode_tool_arguments_passes_through_an_object() {
        let (arguments, error) = decode_tool_arguments(json!({"command": "ls"}));
        assert_eq!(arguments, json!({"command": "ls"}));
        assert!(error.is_none());
    }

    #[test]
    fn decode_tool_arguments_reports_a_truncated_string() {
        // The shape actually observed: generation stopped mid-arguments, so
        // the JSON has no closing quote or brace.
        let truncated = r#"{"command":"cargo test 2>&1"#;
        let (arguments, error) = decode_tool_arguments(Value::String(truncated.to_string()));
        assert_eq!(arguments, Value::String(truncated.to_string()));
        assert!(error.is_some(), "a truncated call must report an error");
    }

    #[test]
    fn decode_tool_arguments_keeps_the_raw_text_when_it_will_not_parse() {
        // The raw text is what the model actually sent, and it is what the
        // conversation has to carry back — the API owes a tool call for every
        // result, and inventing arguments here would misreport the turn.
        let raw = "not json at all";
        let (arguments, error) = decode_tool_arguments(Value::String(raw.to_string()));
        assert_eq!(arguments, Value::String(raw.to_string()));
        assert!(error.is_some());
    }
}

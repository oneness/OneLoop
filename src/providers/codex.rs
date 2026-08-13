//! The Responses API, as ChatGPT's Codex backend speaks it.
//!
//! The second protocol, and the reason `Model::complete` has a `match`. It
//! is not Chat Completions renamed: the conversation is a flat list of typed
//! items rather than roles with attachments, a tool is a function with its
//! fields inline, and the system prompt is a field of its own. The backend
//! only streams, so the answer arrives as server-sent events even though
//! nothing here is displayed until the turn is done.
//!
//! What it buys is the ChatGPT subscription: the same account the Codex CLI
//! uses, reached with the grant `auth/codex.rs` stores.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::messages::{Message, ToolCall};
use crate::auth::codex::ORIGINATOR;
use crate::models::Model;

use super::{
    ProviderHttpError, ProviderRequest, ProviderResponse, STREAM_IDLE_TIMEOUT,
    decode_tool_arguments, extract_error_message,
};

// ── Request ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    /// The backend refuses anything else: this conversation is the client's
    /// to keep, and it keeps none of it.
    store: bool,
    /// Likewise not a choice — the endpoint has no non-streaming mode.
    stream: bool,
    /// Where the system prompt goes; there is no system item.
    instructions: String,
    input: Vec<Item>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
}

#[derive(Debug, Serialize)]
struct Reasoning {
    effort: String,
}

/// A function tool, flat: the Responses API has no nested `function` object.
#[derive(Debug, Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    r#type: &'static str,
    name: String,
    description: String,
    parameters: Value,
    /// Off, because the schemas OneLoop's tools declare are not written to
    /// the subset strict mode accepts.
    strict: bool,
}

/// One entry of the conversation. Everything is an item here — a message, a
/// call, a call's result — which is what makes the list flat.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item {
    Message {
        role: String,
        content: Vec<Content>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        /// A JSON-encoded object, as on the other protocol.
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

/// Input and output text are different types on this API even when the words
/// are the same, and a message is rejected if it uses the wrong one for its
/// role.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content {
    InputText { text: String },
    OutputText { text: String },
}

fn to_items(messages: Vec<Message>) -> Vec<Item> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(Item::Message {
                role: "user".to_string(),
                content: vec![Content::InputText { text: user.content }],
            }),
            // A turn that was only a tool call has no text to replay, and an
            // empty content list is rejected.
            Message::Assistant(assistant) if assistant.content.trim().is_empty() => None,
            Message::Assistant(assistant) => Some(Item::Message {
                role: "assistant".to_string(),
                content: vec![Content::OutputText {
                    text: assistant.content,
                }],
            }),
            Message::ToolCall(call) => Some(Item::FunctionCall {
                call_id: call.id,
                name: call.name,
                arguments: encode_arguments(&call.arguments),
            }),
            Message::ToolResult(result) => Some(Item::FunctionCallOutput {
                call_id: result.tool_call_id,
                output: result.content,
            }),
        })
        .collect()
}

/// Arguments go back as the encoded object the API asks for. A call whose
/// arguments never parsed is replayed as `{}`: the result travelling beside
/// it is what tells the model to try again, and half an argument list is no
/// more use than none.
fn encode_arguments(arguments: &Value) -> String {
    match arguments {
        Value::Object(_) => arguments.to_string(),
        Value::String(text)
            if matches!(serde_json::from_str::<Value>(text), Ok(Value::Object(_))) =>
        {
            text.clone()
        }
        _ => "{}".to_string(),
    }
}

// ── Response ──────────────────────────────────────────────────────────

/// The events a turn is assembled from. The rest are partial views of an
/// item already counted here — deltas, and the `added` half of each `done` —
/// and nothing renders as it streams, so they are skipped.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    /// One finished item: a message, or a call to make. This is where the
    /// turn's content actually arrives.
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OutputItem },
    /// The turn is over. What it carries is not the turn: ChatGPT's backend
    /// closes with an empty `output` and has already sent every item, while
    /// the same event from OpenAI's own API repeats them all. Whichever
    /// arrives is used, so neither is assumed and nothing is counted twice.
    #[serde(rename = "response.completed")]
    Completed { response: CompletedResponse },
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    output: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
struct FailedResponse {
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    message: String,
}

/// What the turn produced. Reasoning arrives as an item too; it is dropped,
/// because the session has nowhere to put it and echoing a partial chain
/// back is worse than sending none.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    Message {
        #[serde(default)]
        content: Vec<OutputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct OutputContent {
    #[serde(default)]
    text: Option<String>,
}

pub async fn complete(model: &Model, request: ProviderRequest) -> Result<ProviderResponse> {
    let tools: Vec<ToolDefinition> = request
        .tools
        .into_iter()
        .map(|tool| ToolDefinition {
            r#type: "function",
            name: tool.name,
            description: tool.description,
            parameters: tool.schema,
            strict: false,
        })
        .collect();

    let body = ResponsesRequest {
        model: model.id.clone(),
        store: false,
        stream: true,
        // Not optional on this API, and an empty one makes the model answer
        // as though it had no instructions at all.
        instructions: request
            .system_prompt
            .unwrap_or_else(|| "You are a helpful coding assistant.".to_string()),
        input: to_items(request.messages),
        // Some models reject `tool_choice` when no tools are offered.
        tool_choice: (!tools.is_empty()).then(|| "auto".to_string()),
        tools,
        parallel_tool_calls: true,
        reasoning: model
            .reasoning_effort
            .clone()
            .map(|effort| Reasoning { effort }),
    };

    let post = model
        .provider
        .post("codex/responses")
        .await?
        // Codex-specific, and required: the backend serves this endpoint
        // only to clients that ask for it by name.
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", ORIGINATOR)
        .header("accept", "text/event-stream")
        .json(&body);

    let (content, tool_calls) = split_output(read_turn(post, &model.alias).await?);
    Ok(ProviderResponse {
        content,
        tool_calls,
    })
}

/// Reads the stream only as far as the event that closes the turn, then
/// drops the connection.
///
/// Not an optimization: the backend leaves the connection open after that
/// event, so reading the body to its end — which is what every other
/// request here does — waits out an idle timeout that has nothing to do
/// with the answer, on a turn that finished seconds earlier.
async fn read_turn(request: reqwest::RequestBuilder, alias: &str) -> Result<Vec<OutputItem>> {
    let mut response = tokio::time::timeout(STREAM_IDLE_TIMEOUT, request.send())
        .await
        .with_context(|| {
            format!(
                "{alias} did not begin responding within {} seconds",
                STREAM_IDLE_TIMEOUT.as_secs()
            )
        })?
        .with_context(|| format!("failed to send request to {alias}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = tokio::time::timeout(STREAM_IDLE_TIMEOUT, response.text())
            .await
            .with_context(|| {
                format!(
                    "{alias} error response was idle for {} seconds",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("failed to read the {alias} error response"))?;
        return Err(ProviderHttpError {
            status,
            message: extract_error_message(&body),
            provider: alias.to_string(),
        }
        .into());
    }

    // Bytes rather than text: a chunk boundary can fall inside a character,
    // and lines are only complete once a newline has arrived.
    let mut pending = Vec::new();
    let mut turn = Turn::default();
    loop {
        let chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, response.chunk())
            .await
            .with_context(|| {
                format!(
                    "{alias} stream was idle for {} seconds",
                    STREAM_IDLE_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("failed to read the {alias} stream"))?;
        let Some(chunk) = chunk else {
            break;
        };
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let event = event(&String::from_utf8_lossy(&line))
                .with_context(|| format!("failed to parse an event from the {alias} stream"))?;
            if let Some(output) = turn.absorb(event, alias)? {
                return Ok(output);
            }
        }
    }
    Err(Turn::incomplete(alias))
}

/// The items seen so far, and why the turn might have ended without them.
#[derive(Debug, Default)]
struct Turn {
    items: Vec<OutputItem>,
}

impl Turn {
    /// `Some` once the turn is closed — nothing after that event belongs to
    /// it, and the connection stays open regardless.
    fn absorb(&mut self, event: Option<Event>, alias: &str) -> Result<Option<Vec<OutputItem>>> {
        match event {
            Some(Event::OutputItemDone { item }) => {
                self.items.push(item);
                Ok(None)
            }
            Some(Event::Completed { response }) => Ok(Some(match response.output.is_empty() {
                true => std::mem::take(&mut self.items),
                false => response.output,
            })),
            Some(Event::Failed { response }) => {
                let message = response
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "the provider gave no reason".to_string());
                Err(anyhow::anyhow!("{alias} refused the request: {message}"))
            }
            _ => Ok(None),
        }
    }

    fn incomplete(alias: &str) -> anyhow::Error {
        // Reached when the connection ends mid-turn: some of the answer
        // arrived, but the event that closes it never did.
        anyhow::anyhow!("{alias} stream ended before the turn completed")
    }
}

/// The event a `data:` line carries. SSE metadata is ignored, but malformed
/// JSON in a data line is a protocol error; silently dropping a closing event
/// would otherwise turn a clear parse failure into an idle timeout.
fn event(line: &str) -> Result<Option<Event>> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    serde_json::from_str(data.trim())
        .map(Some)
        .context("event data was not valid JSON")
}

/// The same read, over a stream already in hand — which is what the tests
/// have.
#[cfg(test)]
fn final_output(stream: &str, alias: &str) -> Result<Vec<OutputItem>> {
    let mut turn = Turn::default();
    for line in stream.lines() {
        if let Some(output) = turn.absorb(event(line)?, alias)? {
            return Ok(output);
        }
    }
    Err(Turn::incomplete(alias))
}

fn split_output(output: Vec<OutputItem>) -> (String, Vec<ToolCall>) {
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item {
            OutputItem::Message { content } => {
                text.extend(content.into_iter().filter_map(|part| part.text));
            }
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let (arguments, parse_error) = decode_tool_arguments(Value::String(arguments));
                tool_calls.push(ToolCall {
                    id: call_id,
                    name,
                    arguments,
                    parse_error,
                });
            }
            OutputItem::Other => {}
        }
    }
    (text.join("\n").trim().to_string(), tool_calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{AssistantMessage, ToolResultMessage, UserMessage};
    use serde_json::json;

    fn items(messages: Vec<Message>) -> Value {
        serde_json::to_value(to_items(messages)).unwrap()
    }

    fn call(arguments: Value) -> Message {
        Message::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments,
            parse_error: None,
        })
    }

    #[test]
    fn a_conversation_becomes_a_flat_list_of_typed_items() {
        let json = items(vec![
            Message::User(UserMessage {
                content: "hi".into(),
            }),
            Message::Assistant(AssistantMessage {
                content: "on it".into(),
            }),
            call(json!({"command": "ls"})),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1".into(),
                tool_name: "bash".into(),
                content: "a.rs".into(),
                is_error: false,
            }),
        ]);
        assert_eq!(
            json,
            json!([
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "on it"}]},
                {"type": "function_call", "call_id": "call_1", "name": "bash",
                 "arguments": r#"{"command":"ls"}"#},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.rs"},
            ])
        );
    }

    /// A tool-calling turn carries no text, and a message item with an empty
    /// content list is rejected by the API.
    #[test]
    fn an_assistant_turn_with_no_text_is_left_out() {
        let json = items(vec![
            Message::Assistant(AssistantMessage {
                content: "  ".into(),
            }),
            call(json!({})),
        ]);
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["type"], "function_call");
    }

    #[test]
    fn arguments_that_never_parsed_are_replayed_as_an_empty_object() {
        let json = items(vec![call(Value::String(r#"{"path":"a.r"#.into()))]);
        assert_eq!(json[0]["arguments"], json!("{}"));
    }

    #[test]
    fn already_encoded_arguments_are_not_encoded_twice() {
        let json = items(vec![call(Value::String(r#"{"path":"a.rs"}"#.into()))]);
        assert_eq!(json[0]["arguments"], json!(r#"{"path":"a.rs"}"#));
    }

    fn stream(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"]))
            .collect()
    }

    #[test]
    /// The shape ChatGPT's backend actually sends: each item arrives as it
    /// finishes, and the event that closes the turn carries an empty
    /// `output`. Reading the answer out of that event alone — which is what
    /// the API's own shape suggests — gets nothing at all.
    fn the_answer_is_assembled_from_the_items_the_turn_sent() {
        let text = stream(&[
            json!({"type": "response.created", "response": {}}),
            json!({"type": "response.output_text.delta", "delta": "wor"}),
            json!({"type": "response.output_item.done", "item":
                {"type": "reasoning", "summary": []}}),
            json!({"type": "response.output_item.done", "item":
                {"type": "message", "role": "assistant", "status": "completed",
                 "content": [{"type": "output_text", "text": "working on it"}]}}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let (content, calls) = split_output(final_output(&text, "codex").unwrap());
        assert_eq!(content, "working on it");
        assert!(calls.is_empty());
    }

    #[test]
    fn tool_calls_are_read_out_of_the_items() {
        // Verbatim, including the item id the backend sends beside the call
        // id: the call is answered by `call_id`, not by that one.
        let text = stream(&[
            json!({"type": "response.output_item.done", "item":
                {"id": "fc_08e0", "type": "function_call", "status": "completed",
                 "call_id": "call_1", "name": "read",
                 "arguments": "{\"path\":\"src/main.rs\"}"}}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let (content, calls) = split_output(final_output(&text, "codex").unwrap());
        assert!(content.is_empty(), "the call is the message");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({"path": "src/main.rs"}));
    }

    /// The same turn from an endpoint that repeats every item in the closing
    /// event instead of leaving it empty. Both spellings have to work, and
    /// neither may count an item twice.
    #[test]
    fn a_turn_repeated_in_the_closing_event_is_not_counted_twice() {
        let item = json!({"type": "message", "role": "assistant",
                          "content": [{"type": "output_text", "text": "hello"}]});
        let text = stream(&[
            json!({"type": "response.output_item.done", "item": item}),
            json!({"type": "response.completed", "response": {"output": [item]}}),
        ]);
        let (content, _) = split_output(final_output(&text, "codex").unwrap());
        assert_eq!(content, "hello");
    }

    /// An item type this does not model must not cost the turn it appears in.
    #[test]
    fn an_unknown_output_item_is_ignored() {
        let text = stream(&[
            json!({"type": "response.output_item.done", "item":
                {"type": "web_search_call", "status": "completed"}}),
            json!({"type": "response.output_item.done", "item":
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "found it"}]}}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let (content, _) = split_output(final_output(&text, "codex").unwrap());
        assert_eq!(content, "found it");
    }

    #[test]
    fn a_failed_turn_reports_what_the_server_said() {
        let text = stream(&[json!({"type": "response.failed", "response": {
            "error": {"message": "usage limit reached"}}})]);
        let err = final_output(&text, "codex").unwrap_err();
        assert!(format!("{err:#}").contains("usage limit reached"));
    }

    #[test]
    fn a_malformed_data_event_is_a_protocol_error() {
        let err = final_output("data: {not json}\n\n", "codex").unwrap_err();
        assert!(
            format!("{err:#}").contains("event data was not valid JSON"),
            "got: {err:#}"
        );
    }

    /// A dropped connection leaves the items with no closing event; saying
    /// so beats reporting half an answer as the whole of it.
    #[test]
    fn a_stream_that_never_completed_is_an_error() {
        let text = stream(&[
            json!({"type": "response.created", "response": {}}),
            json!({"type": "response.output_item.done", "item":
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "half"}]}}),
        ]);
        assert!(final_output(&text, "codex").is_err());
    }
}

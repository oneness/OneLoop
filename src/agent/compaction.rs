use std::env;

use super::messages::{AssistantMessage, Message, UserMessage};

/// Approximate characters per token (conservative for mixed code/prose).
const CHARS_PER_TOKEN: usize = 4;

/// Default context window size in tokens.
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;

/// Default threshold percentage to trigger auto-compaction.
const DEFAULT_COMPACTION_THRESHOLD: u8 = 85;

/// Maximum characters to keep from a tool result when stripping.
const TOOL_RESULT_MAX_CHARS: usize = 200;

/// The prompt sent to the model to generate a compacted summary.
const COMPACTION_PROMPT: &str = r#"Summarize this conversation into a compact handoff document. Include:

- What was accomplished
- Current work in progress (files, functions, state)
- What remains to be done
- Key decisions and why
- Important context, constraints, or user preferences

Be thorough but concise. This is the only context the next session will have."#;

/// Estimate the total token count for a slice of messages.
pub fn estimate_tokens(messages: &[Message], system_prompt_chars: usize) -> usize {
    let msg_chars: usize = messages
        .iter()
        .map(|msg| match msg {
            Message::System(text) => text.len(),
            Message::User(user) => user.content.len(),
            Message::Assistant(assistant) => assistant.content.len(),
            Message::ToolCall(tool_call) => {
                tool_call.name.len() + tool_call.arguments.to_string().len()
            }
            Message::ToolResult(tool_result) => tool_result.content.len(),
        })
        .sum();

    (system_prompt_chars + msg_chars) / CHARS_PER_TOKEN
}

/// Check whether the session has exceeded the compaction threshold.
pub fn should_compact(messages: &[Message], system_prompt_chars: usize) -> bool {
    let threshold: u8 = env::var("ONELOOP_COMPACTION_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_COMPACTION_THRESHOLD);

    let context_window: usize = env::var("ONELOOP_CONTEXT_WINDOW_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);

    let limit_tokens = (context_window as u64 * threshold as u64 / 100) as usize;
    let used_tokens = estimate_tokens(messages, system_prompt_chars);

    used_tokens >= limit_tokens
}

/// Build the compaction prompt to send to the model.
pub fn compaction_user_message() -> String {
    COMPACTION_PROMPT.to_string()
}

/// Strip large tool outputs from messages, keeping user/assistant messages intact.
/// Tool results are truncated to a short summary so the compaction call is fast.
/// Tool calls are kept but with simplified arguments (just the tool name and path/cmd).
pub fn strip_tool_outputs(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| match msg {
            Message::User(user) => Message::User(UserMessage {
                content: user.content.clone(),
            }),
            Message::Assistant(assistant) => Message::Assistant(AssistantMessage {
                content: assistant.content.clone(),
            }),
            Message::ToolCall(tool_call) => {
                // Keep tool calls but summarize arguments to just the key info.
                let summary = summarize_tool_arguments(&tool_call.name, &tool_call.arguments);
                Message::Assistant(AssistantMessage {
                    content: format!("[called {}({})]", tool_call.name, summary),
                })
            }
            Message::ToolResult(tool_result) => {
                let truncated = if tool_result.content.len() > TOOL_RESULT_MAX_CHARS {
                    format!(
                        "{}... ({} chars truncated)",
                        &tool_result.content[..TOOL_RESULT_MAX_CHARS],
                        tool_result.content.len()
                    )
                } else {
                    tool_result.content.clone()
                };
                Message::Assistant(AssistantMessage {
                    content: format!(
                        "[{} result: {}] {}",
                        tool_result.tool_name,
                        if tool_result.is_error { "error" } else { "ok" },
                        truncated
                    ),
                })
            }
            Message::System(text) => Message::System(text.clone()),
        })
        .collect()
}

/// Extract just the useful part of tool arguments for a summary.
fn summarize_tool_arguments(name: &str, arguments: &serde_json::Value) -> String {
    match name {
        "bash" => arguments
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        "read" | "write" | "edit" => arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        "web_search" => arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => arguments.to_string(),
    }
}

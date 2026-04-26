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

/// Maximum tokens of recent user messages to preserve after compaction.
const RECENT_USER_MESSAGES_MAX_TOKENS: usize = 20_000;

/// Prefix prepended to the compaction summary in the new session.
pub const SUMMARY_PREFIX: &str = "\
Another language model started working on this task and produced a handoff summary. \
Use it to build on the work already done and avoid duplicating effort.\n\n";

/// The prompt sent to the model to generate a compacted summary.
const COMPACTION_PROMPT: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work."#;

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
                    let mut end = TOOL_RESULT_MAX_CHARS;
                    while end > 0 && !tool_result.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!(
                        "{}... ({} chars truncated)",
                        &tool_result.content[..end],
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

/// Collect recent user messages from history (most recent first), up to
/// `RECENT_USER_MESSAGES_MAX_TOKENS` tokens. Returns them in chronological
/// order. Skips any messages that look like previous compaction summaries.
pub fn collect_recent_user_messages(messages: &[Message]) -> Vec<String> {
    let max_tokens: usize = env::var("ONELOOP_COMPACT_USER_MSG_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RECENT_USER_MESSAGES_MAX_TOKENS);

    let mut selected: Vec<String> = Vec::new();
    let mut remaining = max_tokens;

    for msg in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        if let Message::User(user) = msg {
            // Skip previous compaction summaries.
            if user.content.starts_with(SUMMARY_PREFIX) {
                continue;
            }
            let tokens = user.content.len() / CHARS_PER_TOKEN;
            if tokens <= remaining {
                selected.push(user.content.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                // Partially include this message (truncate to fit).
                let mut keep_chars = (remaining * CHARS_PER_TOKEN).min(user.content.len());
                // Walk back to a valid UTF-8 boundary.
                while keep_chars > 0 && !user.content.is_char_boundary(keep_chars) {
                    keep_chars -= 1;
                }
                if keep_chars > 0 {
                    selected.push(format!("{}... [truncated]", &user.content[..keep_chars]));
                }
                break;
            }
        }
    }

    // Reverse back to chronological order.
    selected.reverse();
    selected
}

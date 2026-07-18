use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;

use super::messages::{AssistantMessage, Message, UserMessage};
use super::spinner::SpinnerGuard;
use super::{Agent, metrics};
use crate::output::{DIM, GREEN, RED, RESET, YELLOW};
use crate::providers::ProviderRequest;

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

/// Check whether the session is near the context limit and, if so, hand off:
/// summarize the session, distil durable facts into memory.md, and start a
/// fresh session seeded with the summary and recent user messages.
pub async fn auto_compact_if_needed(
    agent: &mut Agent,
    provider_override: Option<&str>,
) -> Result<()> {
    let system_prompt_chars = agent
        .config
        .system_prompt
        .as_ref()
        .map(String::len)
        .unwrap_or(0);

    if !should_compact(agent.session.messages(), system_prompt_chars) {
        return Ok(());
    }

    println!("{YELLOW}  ⚠ context near limit — auto-compacting...{RESET}");

    let tokens_before = estimate_tokens(agent.session.messages(), system_prompt_chars);
    let compact_start = Instant::now();

    let Some(summary) = generate_summary(agent, provider_override).await else {
        return Ok(()); // failure already reported; keep the session running
    };
    extract_memory(agent, &summary).await;
    let preserved = reseed_session(agent, &summary)?;

    let system_prompt_chars_after = agent
        .config
        .system_prompt
        .as_ref()
        .map(String::len)
        .unwrap_or(0);
    let tokens_after = estimate_tokens(agent.session.messages(), system_prompt_chars_after);

    agent.metrics.log(
        "compaction",
        json!({
            "duration_ms": compact_start.elapsed().as_millis(),
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
        }),
    );

    println!(
        "{GREEN}  ✓ compacted — new session: {} ({preserved} recent messages preserved){RESET}",
        agent.session.path().display(),
    );
    println!(
        "{DIM}  ⚠ long threads and multiple compactions can reduce accuracy. use /clear when possible to keep sessions focused.{RESET}"
    );

    Ok(())
}

/// Ask the provider for a handoff summary of the session. Returns None
/// (after reporting) on failure — compaction is best-effort.
async fn generate_summary(agent: &mut Agent, provider_override: Option<&str>) -> Option<String> {
    let spinner = SpinnerGuard::new("compacting...");

    let mut compact_messages = strip_tool_outputs(agent.session.messages());
    compact_messages.push(Message::User(UserMessage {
        content: COMPACTION_PROMPT.to_string(),
    }));

    let request = ProviderRequest {
        system_prompt: agent.config.system_prompt.clone(),
        messages: compact_messages,
        tools: Vec::new(),
        model_override: None,
    };

    let result = agent
        .provider_registry
        .complete_with_retry(
            provider_override,
            request,
            Some(spinner.stop_callback()),
            Some(spinner.start_callback("compacting...")),
        )
        .await;
    spinner.stop();

    match result {
        Ok((_used_provider, response)) => Some(response.content),
        Err(e) => {
            eprintln!("{RED}  ✗ compaction failed: {e:#}{RESET}");
            None
        }
    }
}

/// Distil durable facts from the summary into memory.md and reload the
/// system prompt so new memory is visible for the remainder of this session.
/// Failures warn and are otherwise ignored — memory is a background step
/// that must never block the loop.
async fn extract_memory(agent: &mut Agent, summary: &str) {
    // The summary (not the full context) goes to a second, cheap call.
    // Always uses the default provider — memory is infrastructure, not
    // user-directed, so the provider_override from the prompt must not
    // carry over (it may name a provider that is rate-limited or
    // unconfigured). complete_once: single attempt, no retries, no
    // interactive stdin prompt.
    let memory_request = ProviderRequest {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: memory_extraction_message(summary),
        })],
        tools: Vec::new(),
        model_override: None,
    };
    match agent
        .provider_registry
        .complete_once(agent.provider_registry.active_name(), memory_request)
        .await
    {
        Ok(memory_response) => {
            match append_and_trim_memory(&agent.config.cwd, &memory_response.content) {
                Ok(()) => {
                    agent.config.system_prompt = crate::config::build_system_prompt(
                        &agent.config.cwd,
                        &agent.tool_registry.names(),
                    );
                    agent.config.prompt_sources = crate::config::prompt_sources(&agent.config.cwd);
                }
                Err(e) => {
                    eprintln!("{YELLOW}  ⚠ memory update failed: {e:#}{RESET}");
                }
            }
        }
        Err(e) => {
            eprintln!("{YELLOW}  ⚠ memory extraction failed: {e:#}{RESET}");
        }
    }
}

/// Rotate to a fresh session seeded with recent user messages and the
/// summary. Returns how many recent messages were preserved.
fn reseed_session(agent: &mut Agent, summary: &str) -> Result<usize> {
    let recent_user_messages = collect_recent_user_messages(agent.session.messages());

    agent.session = agent.session.rotate()?;
    agent.metrics = metrics::Metrics::from_session_path(agent.session.path())?;

    for user_msg in &recent_user_messages {
        agent.session.push_user(user_msg.clone())?;
    }
    agent
        .session
        .push_user(format!("{SUMMARY_PREFIX}{summary}"))?;
    agent.session.push_assistant(
        "Understood. I have the context from the previous session. Ready to continue.".to_string(),
    )?;

    Ok(recent_user_messages.len())
}

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
    let threshold: u8 =
        crate::config::env_or("ONELOOP_COMPACTION_THRESHOLD", DEFAULT_COMPACTION_THRESHOLD);

    let context_window: usize = crate::config::env_or(
        "ONELOOP_CONTEXT_WINDOW_TOKENS",
        DEFAULT_CONTEXT_WINDOW_TOKENS,
    );

    let limit_tokens = (context_window as u64 * threshold as u64 / 100) as usize;
    let used_tokens = estimate_tokens(messages, system_prompt_chars);

    used_tokens >= limit_tokens
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
                        crate::output::truncate_at_char_boundary(
                            &tool_result.content,
                            TOOL_RESULT_MAX_CHARS
                        ),
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
    match super::key_argument(name, arguments) {
        Some(argument) => argument.to_string(),
        None => arguments.to_string(),
    }
}

/// The prompt sent to the model to extract memory facts from a compaction summary.
pub const MEMORY_EXTRACTION_PROMPT: &str = r#"You are given a session summary. Extract a short bullet list of facts worth remembering in future sessions: user preferences, project decisions, recurring constraints, or anything the user would not want to repeat explaining.

Rules:
- Each bullet must be one line, starting with "- "
- Only include facts that generalise beyond this single task (skip task-specific details)
- If there is nothing worth keeping, respond with exactly: (nothing)
- Do not include any other text, headers, or explanation"#;

/// The maximum number of lines to keep in memory.md before trimming.
const MEMORY_MAX_LINES: usize = 200;

/// Build the memory extraction user message from a compaction summary.
pub fn memory_extraction_message(summary: &str) -> String {
    format!("Session summary:\n\n{summary}\n\n{MEMORY_EXTRACTION_PROMPT}")
}

/// Append new memory facts to memory.md and trim to MEMORY_MAX_LINES.
/// `new_facts` is the raw model response; lines not starting with "- " are skipped.
/// Does nothing if the response is "(nothing)" or contains no valid bullet lines.
pub fn append_and_trim_memory(cwd: &Path, new_facts: &str) -> Result<()> {
    let trimmed = new_facts.trim();
    if trimmed == "(nothing)" {
        return Ok(());
    }

    // Strip code fences and normalize indentation so the filter is robust
    // against models that wrap output in ```markdown blocks or indent bullets.
    let bullets: Vec<String> = trimmed
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .map(|l| l.trim_start().to_string())
        .filter(|l| l.starts_with("- "))
        .collect();

    if bullets.is_empty() {
        return Ok(());
    }

    let memory_path = crate::config::memory_path(cwd);
    let dir = memory_path
        .parent()
        .with_context(|| format!("memory path has no parent: {}", memory_path.display()))?;
    fs::create_dir_all(dir)?;

    let existing = match fs::read_to_string(&memory_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        // Don't overwrite a file we can't read — propagate the error.
        Err(e) => return Err(e.into()),
    };
    let mut lines: Vec<String> = existing.lines().map(String::from).collect();

    for bullet in bullets {
        lines.push(bullet);
    }

    // Trim oldest lines when over the cap.
    if lines.len() > MEMORY_MAX_LINES {
        let drop = lines.len() - MEMORY_MAX_LINES;
        lines.drain(..drop);
    }

    fs::write(&memory_path, lines.join("\n") + "\n")?;
    Ok(())
}

/// Collect recent user messages from history (most recent first), up to
/// `RECENT_USER_MESSAGES_MAX_TOKENS` tokens. Returns them in chronological
/// order. Skips any messages that look like previous compaction summaries.
pub fn collect_recent_user_messages(messages: &[Message]) -> Vec<String> {
    let max_tokens: usize = crate::config::env_or(
        "ONELOOP_COMPACT_USER_MSG_TOKENS",
        RECENT_USER_MESSAGES_MAX_TOKENS,
    );

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
                let kept = crate::output::truncate_at_char_boundary(
                    &user.content,
                    remaining * CHARS_PER_TOKEN,
                );
                if !kept.is_empty() {
                    selected.push(format!("{kept}... [truncated]"));
                }
                break;
            }
        }
    }

    // Reverse back to chronological order.
    selected.reverse();
    selected
}

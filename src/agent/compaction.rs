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
/// Good enough to size a metric by; never good enough to decide on, which is
/// why nothing branches on it.
const CHARS_PER_TOKEN: usize = 4;

const TOOL_RESULT_MAX_CHARS: usize = 200;

/// Generous next to a tool result, because an assistant turn is reasoning
/// rather than bulk output — but bounded, because it is the one message the
/// model writes without a ceiling of its own. A local server told to predict
/// until the context fills can answer with the whole window, and a summary
/// request carrying that verbatim is refused for exactly the reason it was
/// sent.
const ASSISTANT_MAX_CHARS: usize = 8_000;

const RECENT_USER_MESSAGES_MAX_TOKENS: usize = 20_000;

pub const SUMMARY_PREFIX: &str = "\
Another language model started working on this task and produced a handoff summary. \
Use it to build on the work already done and avoid duplicating effort.\n\n";

const COMPACTION_PROMPT: &str = r#"You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work."#;

/// Summarize, distil to memory.md, and reseed a fresh session with the
/// summary and recent user messages.
///
/// Unconditional: the caller decides when, because only the caller knows
/// whether it was asked for or forced. `Ok(false)` means the summary call
/// itself failed and the session was left exactly as it was — the one
/// outcome a caller that meant to retry afterwards must not ignore.
pub async fn compact(agent: &mut Agent, model_override: Option<&str>) -> Result<bool> {
    let tokens_before = estimate_tokens(agent.session.messages(), agent.system_prompt_chars());
    let compact_start = Instant::now();

    let Some(summary) = generate_summary(agent, model_override).await else {
        return Ok(false); // failure already reported; keep the session running
    };
    extract_memory(agent, &summary).await;
    let preserved = reseed_session(agent, &summary)?;

    // Recompute the prompt length — extract_memory may have reloaded it.
    let tokens_after = estimate_tokens(agent.session.messages(), agent.system_prompt_chars());

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

    Ok(true)
}

/// None (after reporting) on failure — compaction is best-effort.
async fn generate_summary(agent: &mut Agent, model_override: Option<&str>) -> Option<String> {
    let spinner = SpinnerGuard::new("compacting...");

    let mut compact_messages = strip_tool_outputs(agent.session.messages());
    compact_messages.push(Message::User(UserMessage {
        content: COMPACTION_PROMPT.to_string(),
    }));

    let request = ProviderRequest {
        system_prompt: agent.config.system_prompt.clone(),
        messages: compact_messages,
        tools: Vec::new(),
    };

    let result = agent
        .models
        .complete_with_retry(
            model_override,
            request,
            Some(spinner.stop_callback()),
            Some(spinner.start_callback("compacting...")),
        )
        .await;
    spinner.stop();

    match result {
        Ok((_used_model, response)) => Some(response.content),
        Err(e) => {
            eprintln!("{RED}  ✗ compaction failed: {e:#}{RESET}");
            None
        }
    }
}

/// Distil durable facts into memory.md and reload the system prompt.
/// Failures warn and are otherwise ignored — a background step must never
/// block the loop.
async fn extract_memory(agent: &mut Agent, summary: &str) {
    // The active model, not the prompt's override: memory is infrastructure,
    // and the override may name a model that is rate-limited or unconfigured.
    // `complete_once` keeps it to one attempt with no interactive prompt.
    let memory_request = ProviderRequest {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: memory_extraction_message(summary),
        })],
        tools: Vec::new(),
    };
    match agent
        .models
        .complete_once(&agent.models.active().alias, memory_request)
        .await
    {
        Ok(memory_response) => {
            match append_and_trim_memory(&agent.config.cwd, &memory_response.content) {
                Ok(()) => {
                    agent.config.system_prompt = Some(crate::config::build_system_prompt(
                        &agent.config.cwd,
                        &agent.tool_registry.names(),
                    ));
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
                content: truncate_with_note(&assistant.content, ASSISTANT_MAX_CHARS),
            }),
            Message::ToolCall(tool_call) => {
                // Keep tool calls but summarize arguments to just the key info.
                let summary = summarize_tool_arguments(&tool_call.name, &tool_call.arguments);
                Message::Assistant(AssistantMessage {
                    content: format!("[called {}({})]", tool_call.name, summary),
                })
            }
            Message::ToolResult(tool_result) => {
                let truncated = truncate_with_note(&tool_result.content, TOOL_RESULT_MAX_CHARS);
                Message::Assistant(AssistantMessage {
                    content: format!(
                        "[{} result: {}] {}",
                        tool_result.tool_name,
                        if tool_result.is_error { "error" } else { "ok" },
                        truncated
                    ),
                })
            }
        })
        .collect()
}

/// Shorten to `max_chars`, saying how much went. The note is the point:
/// silently shortened text reads to the summarizing model as though that was
/// all there ever was, and it will summarize the gap as fact.
fn truncate_with_note(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let kept = crate::output::truncate_at_char_boundary(text, max_chars);
    format!(
        "{kept}... ({} of {} chars truncated)",
        text.len() - kept.len(),
        text.len()
    )
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

#[cfg(test)]
mod tests {
    use super::{SUMMARY_PREFIX, collect_recent_user_messages, strip_tool_outputs};
    use crate::agent::messages::{AssistantMessage, Message, ToolResultMessage, UserMessage};

    fn user(content: &str) -> Message {
        Message::User(UserMessage {
            content: content.to_string(),
        })
    }

    #[test]
    fn compact_preserves_recent_user_messages_in_order() {
        let messages = vec![user("first"), user("second"), user("third")];
        assert_eq!(
            collect_recent_user_messages(&messages),
            vec!["first", "second", "third"],
            "the newest are chosen, but chronological order is what is replayed"
        );
    }

    #[test]
    fn a_previous_summary_is_not_replayed_as_a_user_message() {
        // Compacting twice would otherwise carry a summary of a summary
        // forward, and grow the thing it exists to shrink.
        let messages = vec![
            user("the actual question"),
            user(&format!("{SUMMARY_PREFIX}an earlier handoff")),
        ];
        assert_eq!(
            collect_recent_user_messages(&messages),
            vec!["the actual question"]
        );
    }

    #[test]
    fn the_newest_user_messages_win_the_budget() {
        let budget_tokens: usize = crate::config::env_or(
            "ONELOOP_COMPACT_USER_MSG_TOKENS",
            super::RECENT_USER_MESSAGES_MAX_TOKENS,
        );
        // One message that alone eats the whole budget, then a newer one.
        let huge = "x".repeat(budget_tokens * super::CHARS_PER_TOKEN);
        let messages = vec![user(&huge), user("what I just asked")];

        let kept = collect_recent_user_messages(&messages);
        assert_eq!(
            kept.last().map(String::as_str),
            Some("what I just asked"),
            "the newest ask is chosen first and replayed last, whatever precedes it"
        );
        assert!(
            kept[0].len() < huge.len(),
            "the oversized predecessor is truncated to what is left, not dropped"
        );
    }

    #[test]
    fn a_runaway_assistant_turn_cannot_oversize_the_summary_request() {
        // A local server predicting until the context fills answers with the
        // whole window. Replaying that verbatim gets the summary call
        // refused for the very reason it was made.
        let runaway = "na ".repeat(50_000);
        let stripped = strip_tool_outputs(&[Message::Assistant(AssistantMessage {
            content: runaway.clone(),
        })]);

        let Some(Message::Assistant(assistant)) = stripped.first() else {
            panic!("an assistant message stays one");
        };
        assert!(assistant.content.len() < runaway.len() / 10);
        assert!(assistant.content.contains("truncated"));
    }

    #[test]
    fn an_ordinary_assistant_turn_is_left_whole() {
        // The cap is for runaways; normal reasoning must reach the
        // summarizer unedited, or compaction quietly degrades every session.
        let answer = "The bug is in the retry path.".repeat(20);
        let stripped = strip_tool_outputs(&[Message::Assistant(AssistantMessage {
            content: answer.clone(),
        })]);

        let Some(Message::Assistant(assistant)) = stripped.first() else {
            panic!("an assistant message stays one");
        };
        assert_eq!(assistant.content, answer);
    }

    #[test]
    fn a_tool_result_is_summarised_rather_than_replayed() {
        // The summary request must not carry the output that overflowed the
        // window in the first place.
        let messages = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "1".to_string(),
            tool_name: "read".to_string(),
            content: "y".repeat(10_000),
            is_error: false,
        })];

        let stripped = strip_tool_outputs(&messages);
        let Some(Message::Assistant(assistant)) = stripped.first() else {
            panic!("a tool result must become an assistant note");
        };
        assert!(assistant.content.len() < 1_000, "output must be truncated");
        assert!(assistant.content.contains("truncated"));
    }
}

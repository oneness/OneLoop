//! Compaction: a conversation replaced by a shorter one.
//!
//! The summarizer is a function, not an agent: text in, text out, no tools and
//! no session. What a function cannot do is decide how much text it can carry,
//! so the decisions that used to be a model's strategy live here — render,
//! chunk, reduce. That is the gain: the same summary by the same steps every
//! time, and every step reachable from a test without a provider.

use std::path::Path;

use anyhow::{Result, bail};

use super::Agent;
use super::metrics;
use super::spinner::SpinnerGuard;
use crate::messages::{Message, UserMessage};
use crate::output;
use crate::prompt;
use crate::providers::ProviderRequest;
use crate::status;

/// A payload longer than this is named rather than quoted: a tool result is
/// usually a file the next conversation will never reread, and a call's
/// arguments are the same file by another name.
const QUOTE_BYTES: usize = 2_000;

/// Rounds of chunk-and-summarize before giving up. A summary that does not
/// shrink is a model ignoring its brief, and three attempts is enough to say so.
const MAX_ROUNDS: usize = 3;

impl Agent {
    /// Replaces this conversation with a summary of itself, so the next request
    /// is small however long this one was. The old file stays on disk, and the
    /// briefing names it — unless this conversation was never written, which a
    /// one-shot's is not.
    pub async fn compact(&mut self) -> Result<()> {
        if self.session.messages().is_empty() {
            bail!("nothing to compact: this conversation is empty");
        }
        // Named when it starts and again when it ends: the summarizer is a
        // request, and a wait with nothing on screen cannot be told from a hang.
        output::running("compacting...");
        // The same start and finish for the mode line, which has no spinner and
        // no trace to read: it says `compacting` where a turn says
        // `computing...`. Held across the whole of this rather than around the
        // summarizer, because the name is still true while the session is
        // replaced underneath it.
        let _compacting = status::Compacting::new();

        // A written conversation leaves a file to name. An ephemeral one leaves
        // nothing: naming the path it would have used would point the next
        // conversation at the project's own transcript, which is someone else's.
        let history = self
            .session
            .root()
            .map(|_| self.session.path().to_path_buf());
        let summary = self.summarize(render(self.session.messages())).await?;

        // Nothing above this line touched the session, so a summary that failed
        // leaves the conversation exactly as it was, and the old advice still
        // holds: `/clear`, or a larger window.
        let path = self.clear_session()?;
        self.session
            .push_user(briefing(&summary, history.as_deref()))?;
        // A conversation that was never written has no new file to announce,
        // but it still finished: the line says so either way.
        let done = if history.is_some() {
            format!("compacting done — new session: {}", path.display())
        } else {
            "compacting done".to_string()
        };
        output::tick(&done);
        Ok(())
    }

    async fn summarize(&self, mut text: String) -> Result<String> {
        let budget = chunk_budget(self.models.active().context_window);

        // A rendered conversation usually fits in one request, because rendering
        // is where the compression happens. The rounds exist for the ones that
        // do not: each hands over summaries rather than transcripts, so the next
        // round's input is a fraction of this one's.
        for _ in 0..MAX_ROUNDS {
            let parts = chunk(&text, budget);
            if parts.len() <= 1 {
                return self.summarize_one(text.trim()).await;
            }

            output::step(&format!("compacting: {} parts", parts.len()));
            let mut summaries = Vec::with_capacity(parts.len());
            for (index, part) in parts.iter().enumerate() {
                output::step(&format!("  part {}/{}", index + 1, parts.len()));
                summaries.push(self.summarize_one(part).await?);
            }
            text = summaries.join("\n\n");
        }

        bail!(
            "the conversation did not compress into one request after {MAX_ROUNDS} passes; \
             start a new conversation (/clear) or switch to a model with a larger window"
        )
    }

    /// One request, one summary — the model the user is paying for, with the
    /// retry and fallback every other request gets.
    async fn summarize_one(&self, text: &str) -> Result<String> {
        let spinner = SpinnerGuard::new("summarizing...");
        let request = ProviderRequest {
            system_prompt: Some(prompt::SUMMARIZER.to_string()),
            session_id: self.session.id(),
            messages: vec![Message::User(UserMessage {
                content: text.to_string(),
            })],
            // A summarizer with tools would have to be trusted to stop using
            // them, and the one thing it must not do is go looking for more.
            // The empty list is the limit; nothing in the prompt enforces it.
            tools: Vec::new(),
            errors_log: self.errors_log.clone(),
        };

        let (_, response) = self
            .models
            .complete_with_retry(
                None,
                request,
                Some(spinner.stop_callback()),
                Some(spinner.start_callback("summarizing...")),
            )
            .await?;
        spinner.stop();

        let summary = response.content.trim().to_string();
        if summary.is_empty() {
            bail!("the summarizer returned nothing");
        }
        Ok(summary)
    }
}

/// Bytes of rendered conversation one summarizer request may carry: half the
/// declared window, because the answer has to fit beside its own input, and a
/// summary that displaces its own tail is no use. A model that declares no
/// window gets a fixed budget rather than an unbounded one.
fn chunk_budget(context_window: Option<u64>) -> usize {
    /// Roughly 16k tokens, at the same three bytes per token the estimate
    /// counts everything else in.
    const WITHOUT_A_WINDOW: usize = 48 * 1024;

    context_window.map_or(WITHOUT_A_WINDOW, |window| {
        window as usize * metrics::BYTES_PER_TOKEN / 2
    })
}

/// The conversation as text, with the bulk left out.
///
/// A tool result is usually a file the next conversation will never reread: what
/// it needs is that the read happened and what came of it, not the bytes. Eliding
/// them here is where most of the compression happens — before any model is
/// called — and it is why most sessions summarize in a single request.
fn render(messages: &[Message]) -> String {
    let mut text = String::new();
    for message in messages {
        match message {
            Message::User(user) => {
                text.push_str(&format!("## User\n\n{}\n\n", user.content.trim()));
            }
            // Prose only: reasoning is how an answer was reached, not what was
            // concluded, and this is read for the conclusion.
            Message::Assistant(assistant) => {
                let content = assistant.content.trim();
                if !content.is_empty() {
                    text.push_str(&format!("## Assistant\n\n{content}\n\n"));
                }
            }
            Message::ToolCall(call) => {
                text.push_str(&format!(
                    "## Tool call: {}\n\n{}\n\n",
                    call.name,
                    quoted(&call.arguments.to_string())
                ));
            }
            Message::ToolResult(result) => {
                let body = if result.content.len() > QUOTE_BYTES {
                    format!("[{} bytes, elided]", result.content.len())
                } else {
                    result.content.trim().to_string()
                };
                let failed = if result.is_error { " (failed)" } else { "" };
                text.push_str(&format!(
                    "## Tool result: {}{failed}\n\n{body}\n\n",
                    result.tool_name
                ));
            }
        }
    }
    text
}

/// A payload too long to quote, cut and marked. The mark matters: a call's
/// arguments arrive as JSON, and a blob that stops mid-token without saying so
/// reads as a small file rather than as a truncated one.
fn quoted(payload: &str) -> String {
    if payload.len() <= QUOTE_BYTES {
        return payload.to_string();
    }
    format!(
        "{}… [{} bytes, elided]",
        output::truncate_at_char_boundary(payload, QUOTE_BYTES),
        payload.len()
    )
}

/// Pieces no larger than `budget`, cut on line boundaries so no message is
/// split. A single line over the budget is passed through rather than dropped:
/// the server is the authority on what fits, and losing it whole is worse.
fn chunk(text: &str, budget: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if !current.is_empty() && current.len() + line.len() + 1 > budget {
            parts.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// What the new conversation opens with: the summary, then — when there is one
/// — where the rest of it is. The pointer is written here rather than by the
/// model, so it cannot be paraphrased into a path that does not resolve.
///
/// A conversation that was never written has no transcript to point at, and
/// saying nothing is worse than saying so: an agent handed a path-shaped hole
/// will go looking for the project's own session, which is someone else's
/// conversation.
fn briefing(summary: &str, history: Option<&Path>) -> String {
    let Some(history) = history else {
        return format!(
            "{}\n\n---\nThis conversation was not written to disk, so there is no earlier \
             transcript to reread.",
            summary.trim()
        );
    };
    format!(
        "{}\n\n---\nRead the full earlier transcript if a detail is missing: {}",
        summary.trim(),
        history.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{AssistantMessage, ToolCall, ToolResultMessage};
    use serde_json::json;

    fn user(content: &str) -> Message {
        Message::User(UserMessage {
            content: content.to_string(),
        })
    }

    #[test]
    fn a_user_message_and_an_answer_both_survive_the_render() {
        let rendered = render(&[
            user("fix the parser"),
            Message::Assistant(AssistantMessage {
                content: "the lexer drops newlines".to_string(),
                reasoning_content: Some("reasoning trace".to_string()),
            }),
        ]);

        assert!(rendered.contains("## User\n\nfix the parser"));
        assert!(rendered.contains("## Assistant\n\nthe lexer drops newlines"));
        assert!(!rendered.contains("reasoning trace"), "{rendered}");
    }

    /// The elision is the compression: a file the model read once is not state,
    /// and quoting it again would cost the summarizer the window it was sent to
    /// save.
    #[test]
    fn a_long_tool_result_is_named_rather_than_quoted() {
        let body = "x".repeat(QUOTE_BYTES + 1);

        let rendered = render(&[Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: body.clone(),
            is_error: false,
        })]);

        assert!(rendered.contains("## Tool result: read"));
        assert!(rendered.contains("bytes, elided"), "{rendered}");
        assert!(!rendered.contains(&body), "the body was quoted anyway");
    }

    #[test]
    fn a_short_tool_result_is_kept_and_a_failure_is_marked() {
        let rendered = render(&[Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            content: "no such file".to_string(),
            is_error: true,
        })]);

        assert!(rendered.contains("## Tool result: bash (failed)"));
        assert!(rendered.contains("no such file"));
    }

    #[test]
    fn a_tool_call_is_rendered_with_its_arguments() {
        let rendered = render(&[Message::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "src/parser.rs"}),
            parse_error: None,
        })]);

        assert!(rendered.contains("## Tool call: read"));
        assert!(rendered.contains("src/parser.rs"));
    }

    /// A call's arguments are JSON, so a cut one is indistinguishable from a
    /// short one unless the cut says so.
    #[test]
    fn a_long_tool_call_is_marked_as_cut() {
        let rendered = render(&[Message::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "write".to_string(),
            arguments: json!({"content": "x".repeat(QUOTE_BYTES + 1)}),
            parse_error: None,
        })]);

        assert!(rendered.contains("bytes, elided"), "{rendered}");
    }

    /// The passed-through part is the point: a line over the budget still
    /// reaches the server, which is the only authority on whether it fits.
    #[test]
    fn a_line_over_the_budget_is_passed_through_not_dropped() {
        let long = "x".repeat(100);

        let parts = chunk(&long, 10);

        assert_eq!(parts.len(), 1);
        assert!(parts[0].contains(&long));
    }

    #[test]
    fn a_text_over_the_budget_is_cut_on_line_boundaries() {
        let text = "aaaa\nbbbb\ncccc\n";

        let parts = chunk(text, 10);

        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0], "aaaa\nbbbb\n");
        assert_eq!(parts[1], "cccc\n");
    }

    #[test]
    fn a_text_within_the_budget_is_one_part() {
        assert_eq!(chunk("aaaa\nbbbb\n", 1_000), vec!["aaaa\nbbbb\n"]);
    }

    /// The briefing is one message the next agent reads first, so it says what
    /// it is and where the rest lives — no preamble above the summary.
    #[test]
    fn the_briefing_is_the_summary_and_a_pointer() {
        let briefing = briefing(
            "  what we are working on  ",
            Some(Path::new("/p/.oneloop/s.jsonl")),
        );

        assert!(briefing.starts_with("what we are working on\n\n---\n"));
        assert!(briefing.contains("/p/.oneloop/s.jsonl"), "{briefing}");
    }

    /// A one-shot writes no transcript, so there is no file to name — and
    /// naming the path the session would have used would send the next
    /// conversation to the project's own, which is not the one summarized.
    #[test]
    fn a_briefing_with_no_transcript_names_no_file() {
        let briefing = briefing("what we are working on", None);

        assert!(!briefing.contains(".oneloop"), "{briefing}");
    }

    /// Half the window, because the answer has to fit beside its own input.
    #[test]
    fn the_budget_is_half_the_declared_window() {
        assert_eq!(chunk_budget(Some(1_000)), 1_500);
    }

    /// A model that declares no window still gets a budget: an unbounded
    /// summarizer request would be a request the server refuses.
    #[test]
    fn a_model_without_a_window_still_gets_a_budget() {
        assert_eq!(chunk_budget(None), 48 * 1024);
    }
}

//! The turn loop: one prompt, one request at a time, until the model stops
//! asking for tools.
//!
//! Split out of `agent.rs` so the loop can be read on its own. It decides
//! nothing about *what* an agent is — the spec did that — and everything about
//! what happens between a prompt and an answer: what a response means, what
//! gets written down, what is printed, and when to stop.

use std::time::Instant;

use anyhow::Result;
use serde_json::json;

use super::Agent;
use super::batch::run;
use super::duplicate_reads::suppress_duplicate_reads;
use super::metrics::estimate_tokens;
use super::spinner::SpinnerGuard;
use super::trace::format_tool_call;
use crate::messages::ToolCall;
use crate::output;
use crate::providers::{ProviderRequest, ProviderResponse, is_context_overflow};
use crate::status::TurnStatus;

/// The sentence a response with nothing in it is answered with. Printed and
/// recorded, so a session that saw it can be read back.
const FALLBACK_SENTENCE: &str =
    "I wasn't able to generate a response. Please try again or rephrase.";

/// What a response is, as far as writing it down goes.
///
/// Four outcomes, and the third is the one a three-variant enum loses: a
/// response that is only tool calls records **nothing**, because an empty
/// assistant message between a call and its results would change the transcript
/// the provider reads back.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Record {
    /// Content arrived. Keep it with its reasoning, and print the content.
    AsIs,
    /// Only reasoning arrived. Keep it; there is nothing to print.
    ReasoningOnly,
    /// Only tool calls arrived. Keep nothing.
    ToolCallsOnly,
    /// Nothing arrived at all. Keep, and print, the fallback sentence.
    Fallback,
}

/// Which of the four a response is.
fn record(response: &ProviderResponse) -> Record {
    let content = !response.content.trim().is_empty();
    match (content, response.reasoning_content.is_some()) {
        (true, _) => Record::AsIs,
        (false, true) => Record::ReasoningOnly,
        // Only the tool-call-only response differs from the fallback, and it
        // must not fold into `AsIs`: it gets no assistant message at all.
        (false, false) if response.tool_calls.is_empty() => Record::Fallback,
        (false, false) => Record::ToolCallsOnly,
    }
}

/// Whether the turn asks for another request: tool calls, and nothing else.
fn continues(response: &ProviderResponse) -> bool {
    !response.tool_calls.is_empty()
}

impl Agent {
    pub async fn run_once(&mut self, prompt: String) -> Result<()> {
        self.run_once_with_model(prompt, None).await
    }

    /// Runs one turn against an explicit model, making it active only after
    /// that model successfully answers.
    ///
    /// `model` is a caller's choice for this turn — `/model <alias>; …`. Which
    /// model the agent works as otherwise was settled when it was built, in the
    /// registry, so there is no second preference to consult here.
    pub async fn run_once_with_model(&mut self, prompt: String, model: Option<&str>) -> Result<()> {
        // Validated before the prompt is written down: a session must not
        // gain a prompt that never ran.
        if let Some(alias) = model {
            self.models.get(alias)?;
        }
        // A single busy→idle pulse for the buffer's mode line, held for the
        // whole turn loop including tools. Drop covers every exit path.
        let _turn_status = TurnStatus::new();
        self.session.push_user(prompt)?;

        // None until a request completes; a fallback answer pins the loop to
        // whichever model actually replied.
        let mut active_model: Option<String> = model.map(str::to_string);

        // Two exits and no fuse, and both of them have already said their
        // piece. A run that never converges cannot grind on for ever either:
        // the transcript is append-only, so it eventually outgrows the window
        // and the server refuses, and a run in the REPL can always be stopped
        // with Ctrl+C.
        loop {
            let Some(response) = self.request(&mut active_model).await? else {
                // The provider refused and said so; there is nothing to add and
                // the run is over, but not in error.
                break;
            };

            self.keep(&response)?;

            if !continues(&response) {
                break;
            }

            self.execute_tool_calls(response.tool_calls).await?;
        }

        Ok(())
    }

    /// One request: assembly, retry and fallback, and the metric that records
    /// what it cost. `Ok(None)` means the provider refused and said so.
    async fn request(&mut self, active: &mut Option<String>) -> Result<Option<ProviderResponse>> {
        let spinner = SpinnerGuard::new("thinking...");
        let tokens_estimated = estimate_tokens(self.session.messages(), self.system_prompt_chars());
        let api_start = Instant::now();
        // For metrics: the model this request is aimed at, which the registry's
        // active one only approximates after a fallback.
        let requested_model = active
            .clone()
            .unwrap_or_else(|| self.models.active().alias.clone());
        let request = ProviderRequest {
            system_prompt: Some(self.system_prompt.clone()),
            session_id: self.session.id(),
            messages: self.session.messages().to_vec(),
            tools: self.spec.tools.definitions(),
            errors_log: self.errors_log.clone(),
        };

        let response = match self
            .models
            .complete_with_retry(
                active.as_deref(),
                request,
                Some(spinner.stop_callback()),
                Some(spinner.start_callback("thinking...")),
            )
            .await
        {
            Ok((used_model, response)) => {
                // Commit only after a successful response, whether it came from
                // the requested model or a fallback.
                self.models.set_active(&used_model)?;
                *active = Some(used_model);
                response
            }
            Err(e) => {
                spinner.stop();
                self.log_api_call(&requested_model, api_start, tokens_estimated, None, false);
                output::fail(&format!("provider error: {e:#}"));
                // Nothing the loop can do about this one, but the cure is one
                // command away and worth naming: the server is the authority on
                // what fits, and it just said no.
                if is_context_overflow(&e) {
                    output::note(
                        "the conversation no longer fits — use /clear to start fresh, \
                         or switch to a model with a larger context window",
                    );
                }
                return Ok(None);
            }
        };
        spinner.stop();

        // `active` was just set to the model that answered, which may differ
        // from the one requested after a fallback.
        let used_model = active.clone().unwrap_or(requested_model);
        self.log_api_call(
            &used_model,
            api_start,
            tokens_estimated,
            response.input_tokens,
            true,
        );
        // The server's own count for this prompt, when it gave one: the gauge
        // prefers it over the estimate until the session moves on.
        self.last_input_tokens = response.input_tokens;

        Ok(Some(response))
    }

    /// Writes one response into the transcript and, when it has words, onto
    /// stdout. The decision is [`record`]'s; this is only what each one means.
    fn keep(&mut self, response: &ProviderResponse) -> Result<()> {
        match record(response) {
            Record::AsIs => {
                self.push_assistant(response)?;
                println!("{}", response.content);
            }
            Record::ReasoningOnly => self.push_assistant(response)?,
            Record::ToolCallsOnly => {}
            Record::Fallback => {
                self.session.push_assistant(FALLBACK_SENTENCE.to_string())?;
                println!("{FALLBACK_SENTENCE}");
            }
        }
        Ok(())
    }

    fn push_assistant(&mut self, response: &ProviderResponse) -> Result<()> {
        self.session.push_assistant_with_reasoning(
            response.content.clone(),
            response.reasoning_content.clone(),
        )
    }

    fn log_api_call(
        &self,
        model: &str,
        started: Instant,
        tokens_estimated: usize,
        input_tokens: Option<u64>,
        success: bool,
    ) {
        self.metrics.log(
            "api_call",
            json!({
                "model": model,
                // A log line is not worth failing over an unknown name.
                "model_id": self.models.get(model)
                    .map_or_else(|_| "?".to_string(), |m| m.id.clone()),
                "duration_ms": started.elapsed().as_millis(),
                "tokens_estimated": tokens_estimated,
                // The server's own count when it reported one; the pair lets
                // the estimate's accuracy be checked against reality.
                "input_tokens": input_tokens,
                "success": success,
            }),
        );
    }

    /// Calls in one batch run concurrently.
    async fn execute_tool_calls(&mut self, tool_calls: Vec<ToolCall>) -> Result<()> {
        let tool_start = Instant::now();

        // Every call is recorded before any result: a result with no call above
        // it is a conversation the provider will reject.
        for tc in &tool_calls {
            self.session
                .push_tool_call(tc.id.clone(), tc.name.clone(), tc.arguments.clone())?;
        }

        let spinner = SpinnerGuard::new("running tools...");
        let mut results = run(self.spec.tools.clone(), &tool_calls, &self.spec.cwd).await;
        spinner.stop();

        self.metrics.log(
            "tool_exec",
            json!({
                "duration_ms": tool_start.elapsed().as_millis(),
                "tools": tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
                "success": results.iter().all(|r| !r.is_error),
            }),
        );

        // Cheap reads, expensive re-sends: a payload already in the transcript
        // is answered with a pointer to it instead of a second copy. Done here
        // rather than inside the tool because this is where the transcript is
        // in hand — and the transcript is the only honest record of what the
        // model can still see.
        let suppressed = suppress_duplicate_reads(
            &self.spec.tools,
            &self.spec.cwd,
            &tool_calls,
            &mut results,
            self.session.messages(),
        );
        if suppressed > 0 {
            output::step(&format!(
                "answered {suppressed} duplicate read(s) with a pointer to the original"
            ));
        }

        for (tc, result) in tool_calls.iter().zip(results) {
            let tool_label = match self.spec.tools.get(&tc.name) {
                Some(tool) => format_tool_call(tool, &tc.arguments, &self.spec.cwd),
                // A name no tool answers to: the call has already failed, and
                // the failure line names the call as the model sent it.
                None => tc.name.clone(),
            };

            self.session.push_tool_result(
                tc.id.clone(),
                tc.name.clone(),
                result.content.clone(),
                result.is_error,
            )?;

            if result.is_error {
                output::fail(&format!("{tool_label} (failed: {})", result.content));
            } else {
                output::tick(&tool_label);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
            parse_error: None,
        }
    }

    fn response(content: &str, reasoning: Option<&str>, calls: Vec<ToolCall>) -> ProviderResponse {
        ProviderResponse {
            content: content.to_string(),
            tool_calls: calls,
            reasoning_content: reasoning.map(str::to_string),
            input_tokens: None,
        }
    }

    /// The four outcomes, and no more. `ToolCallsOnly` is the one a
    /// three-variant enum loses, and it must not fold into `AsIs`: a
    /// tool-call-only response gets no assistant message today.
    #[test]
    fn a_response_is_recorded_as_one_of_four_things() {
        assert_eq!(
            record(&response("done", None, vec![])),
            Record::AsIs,
            "content alone"
        );
        assert_eq!(
            record(&response("done", Some("reasoning"), vec![call()])),
            Record::AsIs,
            "content and reasoning are still content"
        );
        assert_eq!(
            record(&response("", Some("reasoning"), vec![])),
            Record::ReasoningOnly
        );
        assert_eq!(
            record(&response("", None, vec![call()])),
            Record::ToolCallsOnly
        );
        assert_eq!(record(&response("", None, vec![])), Record::Fallback);
    }

    /// Whitespace is not content: a response of blank lines would print blank
    /// lines and record an assistant message saying nothing.
    #[test]
    fn whitespace_is_not_content() {
        assert_eq!(
            record(&response("  \n\t ", None, vec![call()])),
            Record::ToolCallsOnly
        );
    }

    /// Only a tool call asks for another request; reasoning alone finishes the
    /// turn, because there is nothing left to wait for.
    #[test]
    fn only_tool_calls_continue_the_turn() {
        assert!(continues(&response("", None, vec![call()])));
        assert!(continues(&response("done", None, vec![call()])));
        assert!(!continues(&response("done", None, vec![])));
        assert!(!continues(&response("", Some("reasoning"), vec![])));
    }
}

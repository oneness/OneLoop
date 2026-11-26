//! The turn loop: one prompt, one request at a time, until the model stops
//! asking for tools.
//!
//! Split out of `agent.rs` so the loop can be read on its own. It decides
//! nothing about *what* an agent is — the spec did that — and everything about
//! what happens between a prompt and an answer: what a response means, what
//! gets written down, what is printed, and when to stop.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;

use super::Agent;
use super::batch::{run, span};
use super::compact::HandoffJob;
use super::metrics::estimate_tokens;
use super::spinner::SpinnerGuard;
use super::trace::format_tool_call;
use crate::messages::ToolCall;
use crate::models::RetryMode;
use crate::output;
use crate::providers::{ProviderRequest, ProviderResponse, is_context_overflow};
use crate::status::TurnStatus;
use crate::tools::ToolResult;

/// The sentence a response with nothing in it is answered with. Printed and
/// recorded, so a session that saw it can be read back.
const FALLBACK_SENTENCE: &str =
    "I wasn't able to generate a response. Please try again or rephrase.";

/// What a request was for, as its `api_call` line says it. Two words only
/// because there are two kinds of request: a turn, and the background
/// summarizer that writes a handoff. The ledger adds both up together — the
/// cost is the same kind of cost — so this is the one thing that separates
/// them.
pub(super) const TURN: &str = "turn";
pub(super) const SUMMARIZE: &str = "summarize";

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

/// How long work may run before the trace says what it is.
///
/// A start line exists to prove the process is not hung. Anything that finishes
/// inside a second was never in doubt, and naming it while it ran only left the
/// same text on screen twice — once as `▸ what is running`, once as `✓ what
/// ran`. A second is about how long a reader takes to wonder.
const SAY_AFTER: Duration = Duration::from_secs(1);

/// Awaits `work`, calling `announce` only if it is still running after `after`.
///
/// Where the spinner is silent — comint, a pipe, anything `output::plain()` —
/// these lines are the only evidence a long wait is a wait and not a hang, so
/// they are kept for exactly the waits long enough to need them.
async fn when_slow<F, A>(after: Duration, announce: A, work: F) -> F::Output
where
    F: Future,
    A: FnOnce(),
{
    tokio::pin!(work);
    // An `Option` because the announcement fires at most once: taking it is what
    // makes that true for the compiler as well as for the reader.
    let mut announce = Some(announce);
    loop {
        tokio::select! {
            outcome = &mut work => return outcome,
            _ = tokio::time::sleep(after), if announce.is_some() => {
                if let Some(announce) = announce.take() {
                    announce();
                }
            }
        }
    }
}

/// What one request came back with. A refusal is not an answer, and a
/// conversation that outgrew its window is the one refusal the loop can cure.
enum Attempt {
    Answered(ProviderResponse),
    Overflow,
    Refused,
}

/// How a turn ended. An overflow already rotated the session before this
/// returned, so the caller can put the user back at the prompt at once and
/// decide where the handoff work runs.
pub(crate) enum TurnOutcome {
    Finished,
    Handoff(HandoffJob),
}

impl Agent {
    pub async fn run_once(&mut self, prompt: String) -> Result<TurnOutcome> {
        self.run_once_with_model(prompt, None).await
    }

    /// Runs one turn against an explicit model, making it active only after
    /// that model successfully answers.
    ///
    /// `model` is a caller's choice for this turn — `/model <alias>; …`. Which
    /// model the agent works as otherwise was settled when it was built, in the
    /// registry, so there is no second preference to consult here.
    pub async fn run_once_with_model(
        &mut self,
        prompt: String,
        model: Option<&str>,
    ) -> Result<TurnOutcome> {
        // Validated before the prompt is written down: a session must not
        // gain a prompt that never ran.
        if let Some(alias) = model {
            self.models.get(alias)?;
        }
        // A single busy→idle pulse for the buffer's mode line, held for the
        // whole turn loop including tools. Drop covers every exit path.
        let _turn_status = TurnStatus::new();
        // The prompt belongs to this transcript. If the server says it is
        // full, this message goes into the handoff with the rest of it; the
        // fresh session starts with the user's next prompt instead.
        self.session.push_user(prompt)?;

        // None until a request completes; a fallback answer pins the loop to
        // whichever model actually replied.
        let mut active_model: Option<String> = model.map(str::to_string);
        // The turn names itself once, at the first moment it has run long enough
        // to wonder about — not once per request, which would print the same line
        // for every round trip in it.
        let mut announced = false;

        // An overflow ends the turn: the session is rotated before this
        // returns, so the user can put the next prompt into a fresh one while
        // the old conversation is summarized in the background.
        loop {
            let response = match self.request(&mut active_model, &mut announced).await? {
                Attempt::Answered(response) => response,

                Attempt::Overflow => {
                    let Some(job) = self.begin_handoff()? else {
                        output::note(
                            "context full — this message was not answered; this run keeps no \
                             session, so start a new run",
                        );
                        return Ok(TurnOutcome::Finished);
                    };
                    return Ok(TurnOutcome::Handoff(job));
                }

                Attempt::Refused => break,
            };

            self.keep(&response)?;

            if !continues(&response) {
                break;
            }

            self.execute_tool_calls(response.tool_calls).await?;
        }

        Ok(TurnOutcome::Finished)
    }

    /// One request: assembly, retry and fallback, and the metric that records
    /// what it cost.
    ///
    /// `announced` is the turn's, not this request's: the line naming the work
    /// is printed at most once per turn, by whichever request first runs long
    /// enough to be worth naming.
    async fn request(
        &mut self,
        active: &mut Option<String>,
        announced: &mut bool,
    ) -> Result<Attempt> {
        let spinner = SpinnerGuard::new("computing...");
        let tokens_estimated = estimate_tokens(self.session.messages(), self.system_prompt_chars());
        // The spinner is the signal in a terminal; where it is silent the screen
        // shows nothing for the length of a request, which can run for minutes. So
        // the turn says so once, once the silence needs explaining.
        let naming = format!(
            "computing... · ~{}k tokens in current context",
            tokens_estimated / 1000
        );
        self.waiting_on = "a response".to_string();
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

        let response = match when_slow(
            SAY_AFTER,
            || {
                if spinner.is_silent() && !*announced {
                    *announced = true;
                    output::running(&naming);
                }
            },
            self.models.complete_with_retry(
                active.as_deref(),
                request,
                Some(spinner.stop_callback()),
                Some(spinner.start_callback("computing...")),
                RetryMode::Verbose,
                None,
            ),
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
                self.log_api_call(
                    &requested_model,
                    api_start,
                    tokens_estimated,
                    None,
                    false,
                    TURN,
                );
                output::fail(&format!("provider error: {e:#}"));
                // Whether the loop can cure this one is the loop's decision, so
                // the refusal is classified here and acted on there.
                return Ok(if is_context_overflow(&e) {
                    Attempt::Overflow
                } else {
                    Attempt::Refused
                });
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
            TURN,
        );
        // The server's own count for this prompt, when it gave one: the gauge
        // prefers it over the estimate until the session moves on.
        self.last_input_tokens = response.input_tokens;

        Ok(Attempt::Answered(response))
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

    /// One `api_call` line per request, turn or compaction. The shape is the
    /// same and so is what it costs, so the ledger adds them up together and
    /// only `purpose` tells them apart.
    pub(super) fn log_api_call(
        &self,
        model: &str,
        started: Instant,
        tokens_estimated: usize,
        input_tokens: Option<u64>,
        success: bool,
        purpose: &str,
    ) {
        self.metrics.log(
            "api_call",
            json!({
                "model": model,
                // A log line is not worth failing over an unknown name.
                "model_id": self.models.get(model)
                    .map_or_else(|_| "?".to_string(), |m| m.id.clone()),
                "purpose": purpose,
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
        // Every call is recorded before any result: a result with no call above
        // it is a conversation the provider will reject.
        for tc in &tool_calls {
            self.session
                .push_tool_call(tc.id.clone(), tc.name.clone(), tc.arguments.clone())?;
        }

        // A batch of several calls is one request from the model, not several
        // turns: say so once, above the lines the calls will print for themselves.
        if tool_calls.len() > 1 {
            output::step(&format!("{} tool calls in one batch", tool_calls.len()));
        }

        let spinner = SpinnerGuard::new("running tools...");
        // A command that takes a minute must say what it is while it takes it, but
        // only where the spinner is silent and only once the batch has run long
        // enough to be worth naming: each call's own line carries the same label.
        let labels: Vec<String> = tool_calls
            .iter()
            .map(|tc| match self.spec.tools.get(&tc.name) {
                Some(tool) => format_tool_call(tool, &tc.arguments, &self.spec.cwd),
                None => tc.name.clone(),
            })
            .collect();
        // The same label the screen showed, where it showed one: a stop
        // explained afterwards should name the thing that was running, not the
        // category it fell in.
        self.waiting_on = labels.join(" · ");

        let called = when_slow(
            SAY_AFTER,
            || {
                if spinner.is_silent() {
                    for label in &labels {
                        output::running(label);
                    }
                }
            },
            run(self.spec.tools.clone(), &tool_calls, &self.spec.cwd),
        )
        .await;
        spinner.stop();

        // One measurement, two readers: the span the ledger records and the
        // per-call spans the trace prints both come from what each call timed
        // itself, so there is no second clock to disagree with the first.
        let batch = span(&called);
        let spans: Vec<Duration> = called.iter().map(|call| call.elapsed).collect();
        let results: Vec<ToolResult> = called.into_iter().map(|call| call.result).collect();

        self.metrics.log(
            "tool_exec",
            json!({
                "duration_ms": batch.as_millis(),
                // Per call, because a batch says nothing about which of its
                // calls was the slow one — and the span above cannot be
                // checked without them.
                "calls": tool_calls.iter().zip(&spans).map(|(tc, took)| json!({
                    "tool": tc.name,
                    "duration_ms": took.as_millis(),
                })).collect::<Vec<_>>(),
                "tools": tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
                "success": results.iter().all(|r| !r.is_error),
            }),
        );

        for (((tc, result), tool_label), took) in
            tool_calls.iter().zip(results).zip(labels).zip(spans)
        {
            self.session.push_tool_result(
                tc.id.clone(),
                tc.name.clone(),
                result.content.clone(),
                result.is_error,
            )?;

            // The duration follows the name, before the failure text, which is
            // cut to its first line: a refusal is written for the model and can
            // carry an excerpt, while the trace is read by whoever is watching.
            let took = output::elapsed(took);
            if result.is_error {
                output::fail(&format!(
                    "{tool_label} · {took} (failed: {})",
                    output::first_line(&result.content)
                ));
            } else {
                output::tick(&format!("{tool_label} · {took}"));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fast work finishes before the line would be useful, so it gets no line.
    #[tokio::test]
    async fn when_slow_says_nothing_about_work_that_finishes_first() {
        let said = Arc::new(AtomicUsize::new(0));
        let counter = said.clone();

        let outcome = when_slow(
            Duration::from_millis(50),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            std::future::ready(7),
        )
        .await;

        assert_eq!(outcome, 7);
        assert_eq!(said.load(Ordering::SeqCst), 0);
    }

    /// `SAY_AFTER` is the whole point: work that outlasts it is named while it
    /// is still running, and the outcome still comes back untouched.
    #[tokio::test]
    async fn when_slow_names_work_that_outlasts_the_wait() {
        let said = Arc::new(AtomicUsize::new(0));
        let counter = said.clone();

        let outcome = when_slow(
            Duration::from_millis(20),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                "finished"
            },
        )
        .await;

        assert_eq!(outcome, "finished");
        assert_eq!(said.load(Ordering::SeqCst), 1);
    }

    /// Once, however long it runs: a line repeated every second would be the
    /// noise this exists to remove.
    #[tokio::test]
    async fn when_slow_names_work_once_however_long_it_takes() {
        let said = Arc::new(AtomicUsize::new(0));
        let counter = said.clone();

        when_slow(
            Duration::from_millis(10),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            async {
                tokio::time::sleep(Duration::from_millis(120)).await;
            },
        )
        .await;

        assert_eq!(said.load(Ordering::SeqCst), 1);
    }

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

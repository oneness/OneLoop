pub mod messages;
pub mod metrics;
pub mod session;
mod spinner;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::output;
use crate::status::TurnStatus;
use crate::{
    config::Config,
    models::ModelRegistry,
    path::{display_file, display_path, normalize, resolve_path},
    providers::{ProviderRequest, is_context_overflow},
    tools::{ToolRegistry, ToolResult, skill::skill_candidates},
};

use spinner::SpinnerGuard;

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

/// The argument that best names a tool call, and whether that argument is one
/// file. Compositionally it is two rules: the trace spells a file with
/// [`display_path`], and [`file_key`] keys the batch lock on it — so one table
/// keeps the label a reader sees and the lock the batch takes agreeing. `None`
/// for a tool that names no single argument.
fn salient_field(name: &str) -> Option<(&'static str, bool)> {
    match name {
        "bash" => Some(("command", false)),
        "read" | "write" | "edit" => Some(("path", true)),
        _ => None,
    }
}

/// How much of a bash command the trace shows: enough of it to recognise what
/// ran, not enough to fill the screen with a here-document.
const TRACE_WIDTH: usize = 240;

/// What a cut line ends with. One marker and one spelling, so a reader never
/// has to tell an ellipsis from three dots.
const ELISION: &str = "...";

/// One line, always: a multi-line argument would otherwise scroll the
/// spinner's line away mid-run.
fn first_line(argument: &str) -> String {
    match argument.split_once('\n') {
        Some((first, _)) => format!("{}{ELISION}", first.trim_end()),
        None => argument.to_string(),
    }
}

/// A bash command as at most one line of [`TRACE_WIDTH`] characters. The trace
/// records that a command ran, not the command itself — and the model's own
/// words can be a 1,800-character here-document. Newlines become spaces so the
/// budget measures the command rather than its first line, and whatever is
/// dropped — trailing characters, or the line breaks that were flattened — ends
/// the line with [`ELISION`].
fn elide(command: &str) -> String {
    let one_line = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let shown: String = one_line.chars().take(TRACE_WIDTH).collect();
    if shown == one_line && one_line == command.trim() {
        return one_line;
    }
    format!("{}{ELISION}", shown.trim_end())
}

fn format_tool_call(name: &str, arguments: &Value, cwd: &Path) -> String {
    match name {
        "elisp" => arguments
            .get("expression")
            .and_then(Value::as_str)
            .map(|expression| format!("elisp: {}", first_line(expression)))
            .unwrap_or_else(|| name.to_string()),
        "skill" => arguments
            .get("name")
            .and_then(Value::as_str)
            .map(|skill| format!("skill: {}", skill_file(skill, cwd)))
            .unwrap_or_else(|| name.to_string()),
        _ => match salient_field(name) {
            Some((field, is_file)) => {
                // A file is named the way every other tool names it; anything
                // else is quoted as the model sent it — a command cut to the
                // width a trace line has. A known tool that forgot its argument
                // says so rather than naming the working directory.
                let shown = match arguments.get(field).and_then(Value::as_str) {
                    Some(argument) if is_file => first_line(&display_path(cwd, argument)),
                    Some(argument) => elide(argument),
                    None => "?".to_string(),
                };
                format!("{name}: {shown}")
            }
            None => name.to_string(),
        },
    }
}

/// The skill file `SkillTool` loads: the first candidate on disk in the order
/// that tool searches, and a skill found nowhere says so rather than naming a
/// file that does not exist.
fn skill_file(name: &str, cwd: &Path) -> String {
    skill_candidates(cwd, name)
        .iter()
        .find(|candidate| candidate.is_file())
        .map_or_else(
            || format!("{name} (not found)"),
            |path| display_file(cwd, path),
        )
}

/// Runs one batch of tool calls. Calls that name the same file take turns —
/// each holds that file's lock for the whole call — so a batch of edits to one
/// file applies one at a time instead of each reading the original and
/// overwriting the rest. Calls on different files, and tools that cannot be
/// reasoned about by path (bash, elisp), still run together.
async fn run_batch(
    tool_registry: ToolRegistry,
    tool_calls: &[messages::ToolCall],
    cwd: &Path,
) -> Vec<ToolResult> {
    let mut locks: HashMap<PathBuf, Arc<Mutex<()>>> = HashMap::new();

    let handles: Vec<_> = tool_calls
        .iter()
        .map(|tc| {
            let name = tc.name.clone();
            let arguments = tc.arguments.clone();
            // A call whose arguments never decoded cannot be run, but it can be
            // answered. Reported as a tool failure it reaches the model as a
            // result it can act on, instead of ending the run.
            let parse_error = tc.parse_error.clone();
            let ctx = AgentContext {
                cwd: cwd.to_path_buf(),
            };
            let registry = tool_registry.clone();
            let lock = file_key(cwd, &tc.name, &tc.arguments)
                .map(|key| locks.entry(key).or_default().clone());
            tokio::spawn(async move {
                let _held = match lock {
                    Some(lock) => Some(lock.lock_owned().await),
                    None => None,
                };
                match parse_error {
                    Some(err) => bail!(
                        "arguments were not valid JSON ({err}). Send the call \
                         again with complete, valid JSON arguments."
                    ),
                    None => registry.execute(&name, arguments, &ctx).await,
                }
            })
        })
        .collect();

    join_all(handles)
        .await
        .into_iter()
        .map(|res| match res {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => ToolResult {
                content: format!("Tool execution failed: {e:#}"),
                is_error: true,
            },
            Err(join_err) => ToolResult {
                content: format!("Tool task failed: {join_err}"),
                is_error: true,
            },
        })
        .collect()
}

/// The lock key for a tool call: the file it names, resolved and normalized,
/// for the tools that operate on one path. `None` for the rest — bash and
/// elisp can touch anything, so no single path stands for them.
fn file_key(cwd: &Path, name: &str, arguments: &Value) -> Option<PathBuf> {
    // Only a tool whose whole argument is one path has a path to lock.
    let (field, is_file) = salient_field(name)?;
    if !is_file {
        return None;
    }
    let raw = arguments.get(field)?.as_str()?;
    Some(normalize(&resolve_path(cwd, raw)))
}

/// Payloads a `read` has already put in the transcript, keyed by the file they
/// came from and a digest of exactly what the model was shown.
///
/// Derived from the session on every batch rather than kept in a side table:
/// the transcript *is* the cache. `/clear` empties it and the set with it, a
/// resumed session refills it from disk for free, and if tool output is ever
/// elided from the history the pointer disappears along with the content it
/// pointed at. A side table would need invalidating in all three cases, and
/// getting that wrong means telling the model it has seen something it cannot
/// see — the one way this optimisation can make an answer worse.
fn read_payloads_in_context(
    cwd: &Path,
    history: &[messages::Message],
) -> HashSet<(PathBuf, [u8; 32])> {
    let mut paths: HashMap<&str, PathBuf> = HashMap::new();
    let mut seen = HashSet::new();

    for message in history {
        match message {
            messages::Message::ToolCall(call) if call.name == "read" => {
                if let Some(key) = file_key(cwd, &call.name, &call.arguments) {
                    paths.insert(call.id.as_str(), key);
                }
            }
            messages::Message::ToolResult(result)
                if result.tool_name == "read" && !result.is_error =>
            {
                if let Some(key) = paths.get(result.tool_call_id.as_str())
                    && !result.content.is_empty()
                {
                    seen.insert((key.clone(), payload_digest(&result.content)));
                }
            }
            _ => {}
        }
    }

    seen
}

fn payload_digest(content: &str) -> [u8; 32] {
    Sha256::digest(content.as_bytes()).into()
}

/// Replace a `read` whose payload the transcript already holds with a pointer
/// to it, and report how many were answered that way.
///
/// The model loses nothing — the bytes are still above, verbatim and readable —
/// and stops paying to receive them a second time. In an append-only
/// conversation "a second time" means every remaining turn, which is how one
/// duplicated read became 36% of a session's input tokens.
///
/// Identity is the *payload*, not the request: a file that changed underneath
/// cannot produce a matching digest, so a stale pointer is impossible by
/// construction and no mtime bookkeeping is needed. This is an optimisation of
/// tokens, not of I/O — the file is read either way, and re-reading it costs a
/// millisecond against the thousands of tokens a re-send costs.
fn suppress_duplicate_reads(
    cwd: &Path,
    calls: &[messages::ToolCall],
    results: &mut [ToolResult],
    history: &[messages::Message],
) -> usize {
    let mut seen = read_payloads_in_context(cwd, history);
    let mut suppressed = 0;

    for (call, result) in calls.iter().zip(results.iter_mut()) {
        if call.name != "read" || result.is_error || result.content.is_empty() {
            continue;
        }
        // An escape hatch, not a flag the tool needs: some models re-read on
        // purpose to bring material back into recent attention, and that is a
        // legitimate thing to want. Denying it would trade a token saving for
        // a behaviour change; taxing it by one call does not.
        let forced = call.arguments.get("force").and_then(Value::as_bool) == Some(true);

        let Some(key) = file_key(cwd, &call.name, &call.arguments) else {
            continue;
        };
        let entry = (key, payload_digest(&result.content));

        if !forced && seen.contains(&entry) {
            result.content = duplicate_read_notice(&entry.0, &result.content, cwd);
            suppressed += 1;
        }
        // Recorded either way, so a forced re-read still guards the next one,
        // and so a twin in the same batch — which cannot see the other's
        // result in the history — is still answered with a pointer.
        seen.insert(entry);
    }

    suppressed
}

/// What a suppressed read says instead of its payload: that the bytes are
/// still above, how to reach whatever the copy there did not cover, and how to
/// insist on them anyway.
///
/// It names the file and not the message the copy sits at. The model has no
/// message numbers to count against, so a number would be an unanswerable
/// puzzle in the one sentence the model has to trust; what it needs to know is
/// that the content it asked for is already in the conversation, which is what
/// the second line says. Nothing here claims the earlier copy was the whole
/// file — it may have been one window of it, and the hint below is how the
/// model learns that.
fn duplicate_read_notice(path: &Path, payload: &str, cwd: &Path) -> String {
    let shown = path.strip_prefix(cwd).unwrap_or(path).display();
    let mut notice = format!(
        "read {shown}\nByte-identical to an earlier read of {shown} in this \
         conversation — not re-sent."
    );

    // The earlier copy's own notice is the one line of the payload worth
    // keeping: it is what tells the model how to reach the part it still does
    // not have. Carrying it over beats inventing a second, subtly different
    // hint — and [`output::window_notice`] decides what counts as one, so a
    // notice shape the tool learns to write is carried here for free.
    if let Some(hint) = payload.lines().next_back().and_then(output::window_notice) {
        notice.push('\n');
        notice.push_str(hint);
    }
    notice.push_str("\nPass force:true to re-send it anyway.");
    notice
}

/// Why the per-prompt loop returned. `Resolved` and `Failed` have already
/// said their piece — an answer was given, or an error was printed — so the
/// cap is the only exit that would otherwise end a run in silence.
#[derive(Clone, Copy)]
enum Stop {
    /// The model answered without asking for more tools.
    Resolved,
    /// The provider refused; its error is already on the terminal.
    Failed,
    /// The iteration cap ran out with tool calls still coming.
    Cap,
}

/// The one line a run the cap cut short prints. `None` for the exits that
/// spoke for themselves.
fn cap_notice(stop: Stop, max_iterations: usize) -> Option<String> {
    matches!(stop, Stop::Cap).then(|| {
        format!(
            "iteration cap reached ({max_iterations}) — this run stopped mid-task; \
             send another prompt to continue, or raise ONELOOP_MAX_ITERATIONS"
        )
    })
}

pub struct Agent {
    config: Config,
    models: Arc<ModelRegistry>,
    tool_registry: ToolRegistry,
    session: session::Session,
    metrics: metrics::Metrics,
    /// Input tokens the server reported for the last successful turn, when it
    /// reported any. Cleared on `/clear`; until a turn runs the gauge falls
    /// back to `estimate_tokens`.
    last_input_tokens: Option<u64>,
}

impl Agent {
    pub fn new(config: Config, models: ModelRegistry, tool_registry: ToolRegistry) -> Result<Self> {
        let mut session = session::Session::open_or_create(&config.cwd)?;
        // A previous process may have been killed mid-run, leaving tool
        // calls without results that providers would reject.
        let repaired = session.repair_dangling_tool_calls()?;
        if repaired > 0 {
            output::step(&format!(
                "closed {repaired} interrupted tool call(s) from a previous run"
            ));
        }
        let metrics = metrics::Metrics::from_session_path(session.path())?;
        Ok(Self {
            config,
            models: Arc::new(models),
            tool_registry,
            session,
            metrics,
            last_input_tokens: None,
        })
    }

    /// A one-shot run: no session file is opened, read, or written, and no
    /// metrics are recorded. The conversation exists only for this turn.
    pub fn ephemeral(config: Config, models: ModelRegistry, tool_registry: ToolRegistry) -> Self {
        let session = session::Session::in_memory(&config.cwd);
        Self {
            config,
            models: Arc::new(models),
            tool_registry,
            session,
            metrics: metrics::Metrics::disabled(),
            last_input_tokens: None,
        }
    }

    /// Visible history can be shared with an explicitly requested reviewer.
    pub fn messages(&self) -> &[messages::Message] {
        self.session.messages()
    }

    /// Working directory shared by tools and external reviewers.
    pub fn cwd(&self) -> &std::path::Path {
        &self.config.cwd
    }

    /// Record external reference material without starting a model turn.
    pub fn record_reference(&mut self, reference: String) -> Result<()> {
        self.session.push_user(reference)
    }

    pub fn models(&self) -> &ModelRegistry {
        &self.models
    }

    /// Rebuilds the model registry from disk without disturbing the session.
    /// The replacement happens only after the complete configuration is valid.
    pub fn reload_models(&mut self) -> Result<()> {
        let active = self.models.active().alias.clone();
        let models = ModelRegistry::new()?;

        // Keep the user's session choice when it still exists. Otherwise the
        // freshly loaded config's default is already active.
        if models.get(&active).is_ok() {
            models.set_active(&active)?;
        }

        self.models = Arc::new(models);
        Ok(())
    }

    /// A cancelled run leaves calls without results, which the provider
    /// rejects on the next request.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<()> {
        let repaired = self.session.repair_dangling_tool_calls()?;
        if repaired > 0 {
            output::step(&format!("closed {repaired} interrupted tool call(s)"));
        }
        Ok(())
    }

    /// Rotates to a new file; the old one stays on disk.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::from_session_path(self.session.path())?;
        // The measured count belonged to the conversation just rotated away.
        self.last_input_tokens = None;
        output::step(&format!(
            "cleared context, new session: {}",
            self.session.path().display()
        ));
        Ok(())
    }

    pub async fn run_once(&mut self, prompt: String) -> Result<()> {
        self.run_once_with_model(prompt, None).await
    }

    /// Runs one turn against an explicit model, making it active only after
    /// that model successfully answers.
    pub async fn run_once_with_model(&mut self, prompt: String, model: Option<&str>) -> Result<()> {
        if let Some(alias) = model {
            self.models.get(alias)?;
        }
        // A single busy→idle pulse for the buffer's mode line, held for the
        // whole turn loop including tools. Drop covers every exit path.
        let _turn_status = TurnStatus::new();
        self.session.push_user(prompt)?;

        let max_iterations: usize = crate::config::env_or(
            "ONELOOP_MAX_ITERATIONS",
            crate::config::DEFAULT_MAX_ITERATIONS,
        );

        // None until a request completes; a fallback answer pins the loop to
        // whichever model actually replied.
        let mut active_model: Option<String> = model.map(str::to_string);

        // Pessimistic until a break says otherwise: falling off the end of the
        // range means the cap ran out, and that is the exit with nothing else
        // to announce it.
        let mut stop = Stop::Cap;

        for _iteration in 1..=max_iterations {
            let spinner = SpinnerGuard::new("thinking...");
            let tokens_estimated =
                metrics::estimate_tokens(self.session.messages(), self.system_prompt_chars());
            let api_start = Instant::now();
            // For metrics: the model this request is aimed at, which the
            // registry's active one only approximates after a fallback.
            let requested_model = active_model
                .clone()
                .unwrap_or_else(|| self.models.active().alias.clone());
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                session_id: self.session.id(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
            };

            let response = match self
                .models
                .complete_with_retry(
                    active_model.as_deref(),
                    request,
                    Some(spinner.stop_callback()),
                    Some(spinner.start_callback("thinking...")),
                )
                .await
            {
                Ok((used_model, response)) => {
                    // Commit only after a successful response, whether it came
                    // from the requested model or a fallback.
                    self.models.set_active(&used_model)?;
                    active_model = Some(used_model);
                    response
                }
                Err(e) => {
                    spinner.stop();
                    self.log_api_call(&requested_model, api_start, tokens_estimated, None, false);
                    output::fail(&format!("provider error: {e:#}"));
                    // Nothing the loop can do about this one, but the cure is
                    // one command away and worth naming: the server is the
                    // authority on what fits, and it just said no.
                    if is_context_overflow(&e) {
                        output::note(
                            "the conversation no longer fits — use /clear to start fresh, \
                             or switch to a model with a larger context window",
                        );
                    }
                    stop = Stop::Failed;
                    break;
                }
            };
            spinner.stop();

            // active_model was just set to the model that answered (it may
            // differ from the requested one after a fallback).
            let used_model = active_model.clone().unwrap_or(requested_model);
            self.log_api_call(
                &used_model,
                api_start,
                tokens_estimated,
                response.input_tokens,
                true,
            );
            // The server's own count for this prompt, when it gave one: the
            // gauge prefers it over the estimate until the session moves on.
            self.last_input_tokens = response.input_tokens;

            if !response.content.trim().is_empty() || response.reasoning_content.is_some() {
                self.session.push_assistant_with_reasoning(
                    response.content.clone(),
                    response.reasoning_content.clone(),
                )?;
                if !response.content.trim().is_empty() {
                    println!("{}", response.content);
                }
            } else if response.tool_calls.is_empty() {
                let msg = "I wasn't able to generate a response. Please try again or rephrase.";
                self.session.push_assistant(msg.to_string())?;
                println!("{msg}");
            }

            if response.tool_calls.is_empty() {
                stop = Stop::Resolved;
                break;
            }

            self.execute_tool_calls(response.tool_calls).await?;
        }

        if let Some(notice) = cap_notice(stop, max_iterations) {
            output::warn(&notice);
        }

        Ok(())
    }

    fn system_prompt_chars(&self) -> usize {
        self.config
            .system_prompt
            .as_ref()
            .map(String::len)
            .unwrap_or(0)
    }

    /// What the prompt's gauge shows: how much of the context is still free.
    /// Prefers the prompt-token count the server reported for the last turn,
    /// which is measured rather than approximated; falls back to the byte
    /// estimate until a turn has run. `None` when the active model declares no
    /// window — the number would be fiction without one.
    pub fn remaining_context(&self) -> Option<u8> {
        let window = self.models.active().context_window?;
        Some(match self.last_input_tokens {
            Some(tokens) => metrics::remaining_context_from_tokens(tokens, window),
            None => metrics::remaining_context(
                self.session.messages(),
                self.system_prompt_chars(),
                window,
            ),
        })
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
    async fn execute_tool_calls(&mut self, tool_calls: Vec<messages::ToolCall>) -> Result<()> {
        let tool_start = Instant::now();

        for tc in &tool_calls {
            self.session
                .push_tool_call(tc.id.clone(), tc.name.clone(), tc.arguments.clone())?;
        }

        let spinner = SpinnerGuard::new("running tools...");

        let mut results =
            run_batch(self.tool_registry.clone(), &tool_calls, &self.config.cwd).await;

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
            &self.config.cwd,
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
            let tool_label = format_tool_call(&tc.name, &tc.arguments, &self.config.cwd);

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

    pub fn summary(&self) -> String {
        let model = self.models.active();
        let available = packed(&self.models.by_provider());
        let message_count = self.session.messages().len();
        let tools = self.tool_registry.names().join(", ");
        let context = if self.config.prompt_sources.is_empty() {
            "none".to_string()
        } else {
            self.config.prompt_sources.join(", ")
        };

        let session_state = if message_count > 0 {
            format!("{message_count} messages")
        } else {
            "new".to_string()
        };
        let session_path = display_file(self.cwd(), self.session.path());

        format!(
            "\x1b[1mmodel\x1b[22m    : {model}\n\
             \x1b[1mavailable\x1b[22m: {available}\n\
             \x1b[1mtools\x1b[22m    : {tools}\n\
             \x1b[1msession\x1b[22m  : {session_path} ({session_state})\n\
             \x1b[1mcontext\x1b[22m  : {context}"
        )
    }
}

/// Every summary label is a word, padding, and a colon, so each value starts in
/// the same column; wrapped values keep it and the block stays square.
const VALUE_COLUMN: usize = 11;

/// Wider than a realistic model list, narrow enough to read on a laptop.
const SUMMARY_WIDTH: usize = 80;

/// Provider groups packed into as few lines as fit, each continuation indented
/// to the value column and opened by a comma, so a break reads as "more
/// follows". A group never splits: a provider keeps its aliases together.
fn packed(groups: &[String]) -> String {
    // One column is held back for the comma a wrapped line ends with.
    let budget = SUMMARY_WIDTH - VALUE_COLUMN - 1;
    let mut lines: Vec<String> = Vec::new();
    for group in groups {
        match lines.last_mut() {
            Some(line) if line.len() + 2 + group.len() <= budget => {
                line.push_str(", ");
                line.push_str(group);
            }
            _ => lines.push(group.clone()),
        }
    }
    let indent = format!("\n{}", " ".repeat(VALUE_COLUMN));
    lines.join(&format!(",{indent}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn elisp_tool_call_shows_the_expression() {
        assert_eq!(
            format_tool_call(
                "elisp",
                &serde_json::json!({"expression": "(+ 1 2)"}),
                Path::new("/repo")
            ),
            "elisp: (+ 1 2)"
        );
    }

    #[test]
    fn elisp_tool_call_shows_only_the_first_expression_line() {
        assert_eq!(
            format_tool_call(
                "elisp",
                &serde_json::json!({"expression": "(progn\n  (+ 1 2))"}),
                Path::new("/repo")
            ),
            "elisp: (progn..."
        );
    }

    /// A skill found inside the project is named the way every other file in
    /// the trace is: from the project root, not by its absolute path.
    #[test]
    fn skill_tool_call_shows_its_markdown_filename() {
        let dir = project_dir("skill-label");
        let skill = dir.join(".oneloop").join("skills").join("emacs");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "how to reason").unwrap();

        assert_eq!(
            format_tool_call("skill", &serde_json::json!({"name": "emacs"}), &dir),
            "skill: .oneloop/skills/emacs/SKILL.md"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_skill_falls_back_to_a_visible_failure() {
        let dir = project_dir("skill-missing");
        assert_eq!(
            format_tool_call("skill", &serde_json::json!({"name": "no-such-skill"}), &dir),
            "skill: no-such-skill (not found)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The point of the rule: one file, one spelling. Five shapes of the same
    /// path and three tools that touch it all print the same line.
    #[test]
    fn every_shape_of_one_path_is_spelled_the_same_way() {
        for path in [
            "src/main.rs",
            "./src/main.rs",
            "@src/main.rs",
            "/repo/src/main.rs",
            "src/../src/main.rs",
        ] {
            for tool in ["read", "write", "edit"] {
                assert_eq!(
                    format_tool_call(
                        tool,
                        &serde_json::json!({ "path": path }),
                        Path::new("/repo")
                    ),
                    format!("{tool}: src/main.rs"),
                    "{tool} was given {path}"
                );
            }
        }
    }

    /// A command is not a path: bash keeps the model's words verbatim.
    #[test]
    fn a_command_is_never_treated_as_a_path() {
        assert_eq!(
            format_tool_call(
                "bash",
                &serde_json::json!({"command": "cargo test"}),
                Path::new("/repo")
            ),
            "bash: cargo test"
        );
    }

    /// A command the model wrote as a whole script is cut to one readable
    /// trace line, and the cut says so.
    #[test]
    fn a_long_command_is_elided_at_the_trace_width() {
        let command = "x".repeat(400);
        assert_eq!(
            format_tool_call(
                "bash",
                &serde_json::json!({ "command": command }),
                Path::new("/repo")
            ),
            format!("bash: {}...", "x".repeat(TRACE_WIDTH))
        );
    }

    /// The cut counts characters, not bytes: half a multi-byte character would
    /// be mojibake on the terminal.
    #[test]
    fn elision_never_splits_a_character() {
        let command = "é".repeat(TRACE_WIDTH + 10);
        let label = format_tool_call(
            "bash",
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        assert_eq!(label, format!("bash: {}...", "é".repeat(TRACE_WIDTH)));
    }

    /// A command that fits is printed whole — no marker, nothing removed.
    #[test]
    fn a_command_at_the_width_is_printed_whole() {
        let command = "y".repeat(TRACE_WIDTH);
        let label = format_tool_call(
            "bash",
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        assert_eq!(label, format!("bash: {command}"));
    }

    /// A here-document is flattened into the one line the trace has, and the
    /// budget spans the whole command rather than stopping at the first line
    /// break: a script's opening line is not the command.
    #[test]
    fn a_multiline_command_spends_the_width_budget_across_its_lines() {
        assert_eq!(
            format_tool_call(
                "bash",
                &serde_json::json!({"command": "cd /repo && python3 - <<'PY'\np = 'x'\nPY"}),
                Path::new("/repo")
            ),
            "bash: cd /repo && python3 - <<'PY' p = 'x' PY..."
        );
    }

    /// The cut falls inside the command, not at its first line break, so the
    /// line is as wide as the budget allows.
    #[test]
    fn a_long_multiline_command_is_cut_at_the_width() {
        let command = format!("cd /repo && python3 - <<'PY'\n{}", "p = 'x'\n".repeat(40));
        let label = format_tool_call(
            "bash",
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        let shown = label.strip_prefix("bash: ").unwrap_or_default();
        assert!(shown.ends_with("..."), "{label}");
        assert_eq!(
            shown.trim_end_matches('.').chars().count(),
            TRACE_WIDTH,
            "{label}"
        );
    }

    /// A known tool that forgot its argument says so rather than naming the
    /// working directory.
    #[test]
    fn a_missing_path_argument_stays_a_question_mark() {
        assert_eq!(
            format_tool_call("read", &serde_json::json!({}), Path::new("/repo")),
            "read: ?"
        );
    }

    /// Only the cap goes unreported by the loop itself, so only the cap gets a
    /// line — a resolved or failed run must not be told it ran out of budget.
    #[test]
    fn the_cap_is_the_only_stop_that_reports_itself() {
        assert!(cap_notice(Stop::Resolved, 72).is_none());
        assert!(cap_notice(Stop::Failed, 72).is_none());
        assert!(cap_notice(Stop::Cap, 72).is_some_and(|notice| notice.contains("72")));
    }

    #[test]
    fn a_short_provider_list_stays_on_one_line() {
        let groups = ["local (qwen)".to_string(), "openai (astra)".to_string()];
        assert_eq!(packed(&groups), "local (qwen), openai (astra)");
    }

    #[test]
    fn a_long_provider_list_wraps_under_the_value_column() {
        let groups = [
            "local (qwen)".to_string(),
            "openai (astra, luna)".to_string(),
            "opencode-go (deepseek-flash, meta-muse, mimo-flash)".to_string(),
            "openrouter (laguna, ling, nvidia)".to_string(),
        ];
        let expected = [
            "local (qwen), openai (astra, luna),",
            "           opencode-go (deepseek-flash, meta-muse, mimo-flash),",
            "           openrouter (laguna, ling, nvidia)",
        ]
        .join("\n");
        assert_eq!(packed(&groups), expected);
    }

    fn edit_call(id: &str, path: &str, old: &str, new: &str) -> messages::ToolCall {
        messages::ToolCall {
            id: id.to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({ "path": path, "old_text": old, "new_text": new }),
            parse_error: None,
        }
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-agent-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_call(id: &str, path: &str) -> messages::ToolCall {
        messages::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": path }),
            parse_error: None,
        }
    }

    /// A transcript holding one completed read of `path` whose payload was
    /// `body`, exactly as a previous turn would have left it.
    fn transcript_with_one_read(path: &str, body: &str) -> Vec<messages::Message> {
        vec![
            messages::Message::ToolCall(read_call("earlier", path)),
            messages::Message::ToolResult(messages::ToolResultMessage {
                tool_call_id: "earlier".to_string(),
                tool_name: "read".to_string(),
                content: body.to_string(),
                is_error: false,
            }),
        ]
    }

    fn duplicate_payload() -> &'static str {
        "fn main() {}\n[showing lines 1-1 of 400 · continue with offset=2]"
    }

    /// The case that cost 36% of a session's input tokens: the same file read
    /// twice, the second copy carried on every remaining turn. The model keeps
    /// the content — from the first copy — and gets a pointer instead of the
    /// second.
    #[test]
    fn a_repeated_read_is_answered_with_a_pointer_to_the_original() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        let suppressed = suppress_duplicate_reads(cwd, &calls, &mut results, &history);

        assert_eq!(suppressed, 1);
        assert!(
            results[0].content.contains("src/main.rs"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("not re-sent"),
            "{}",
            results[0].content
        );
        assert!(
            !results[0].content.contains("fn main"),
            "payload was re-sent"
        );
        // The hint to reach the part the model does not have survives, and so
        // does the way to insist on the payload.
        assert!(
            results[0].content.contains("continue with offset=2"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("force:true"),
            "{}",
            results[0].content
        );
    }

    /// A payload that ended at the end of its own window carried no hint, and
    /// the pointer must not invent one — nor claim a completeness the earlier
    /// copy may not have had.
    #[test]
    fn a_pointer_to_a_payload_with_no_hint_claims_nothing_about_the_file() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "fn main() {}".to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", "fn main() {}");

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            1
        );
        assert!(
            !results[0].content.contains("complete file"),
            "{}",
            results[0].content
        );
        assert!(
            !results[0].content.contains("offset="),
            "{}",
            results[0].content
        );
    }

    /// Every notice the `read` tool can end a payload with survives the
    /// pointer, including the one for a line too wide for any window to reach
    /// its end — dropping that one would tell the model it has the whole file
    /// when the only way to see the rest is `bash`.
    #[test]
    fn a_hint_that_no_window_reaches_the_rest_survives_the_pointer() {
        let long = "x".repeat(64);
        let payload = output::truncate_window(&[long.as_str(), "b"], 1, 2, 8);
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "wide.txt")];
        let mut results = vec![ToolResult {
            content: payload.clone(),
            is_error: false,
        }];
        let history = transcript_with_one_read("wide.txt", &payload);

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            1
        );
        assert!(
            results[0]
                .content
                .contains("no line window reaches the rest"),
            "{}",
            results[0].content
        );
        assert!(
            results[0].content.contains("use bash"),
            "{}",
            results[0].content
        );
    }

    /// Nothing is suppressed unless the bytes are genuinely still above: a
    /// file that changed between reads must come back whole, or the model is
    /// told it has content that no longer exists.
    #[test]
    fn a_changed_file_is_never_answered_with_a_pointer() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "fn main() { changed() }".to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            0
        );
        assert_eq!(results[0].content, "fn main() { changed() }");
    }

    /// The same file under a different spelling is the same file: the guard
    /// keys on the resolved path, so `./src/main.rs` and `src/main.rs` meet.
    #[test]
    fn the_guard_keys_on_the_resolved_path() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "./src/../src/main.rs")];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            1
        );
    }

    /// Re-reading to refresh attention is a real behaviour, so the payload is
    /// taxed rather than forbidden: `force` gets it back untouched.
    #[test]
    fn force_brings_the_payload_back() {
        let cwd = Path::new("/repo");
        let mut call = read_call("now", "src/main.rs");
        call.arguments = serde_json::json!({ "path": "src/main.rs", "force": true });
        let calls = vec![call];
        let mut results = vec![ToolResult {
            content: duplicate_payload().to_string(),
            is_error: false,
        }];
        let history = transcript_with_one_read("src/main.rs", duplicate_payload());

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            0
        );
        assert_eq!(results[0].content, duplicate_payload());
    }

    /// Two identical reads in one batch run concurrently, so neither can see
    /// the other in the transcript. The second is still answered with a
    /// pointer, because the first one's payload is in the batch's results.
    #[test]
    fn a_twin_read_in_one_batch_is_answered_with_a_pointer_too() {
        let cwd = Path::new("/repo");
        let calls = vec![
            read_call("first", "src/main.rs"),
            read_call("second", "src/main.rs"),
        ];
        let mut results = vec![
            ToolResult {
                content: duplicate_payload().to_string(),
                is_error: false,
            },
            ToolResult {
                content: duplicate_payload().to_string(),
                is_error: false,
            },
        ];

        let suppressed = suppress_duplicate_reads(cwd, &calls, &mut results, &[]);

        assert_eq!(suppressed, 1);
        assert_eq!(results[0].content, duplicate_payload());
        assert!(
            results[1].content.contains("not re-sent"),
            "{}",
            results[1].content
        );
    }

    /// A failed read carries no payload worth pointing at, and a pointer to a
    /// failure would read as one more failure.
    #[test]
    fn a_failed_read_is_never_answered_with_a_pointer() {
        let cwd = Path::new("/repo");
        let calls = vec![read_call("now", "src/main.rs")];
        let mut results = vec![ToolResult {
            content: "file not found: /repo/src/main.rs".to_string(),
            is_error: true,
        }];
        let history = transcript_with_one_read("src/main.rs", "file not found: /repo/src/main.rs");

        assert_eq!(
            suppress_duplicate_reads(cwd, &calls, &mut results, &history),
            0
        );
    }

    /// Two edits to one file in a single batch must take turns: the second
    /// reads what the first wrote. Run at once, each would read the original
    /// and one edit would be silently lost. The two spellings only meet after
    /// normalization, so this also holds the lock's key honest.
    #[tokio::test]
    async fn edits_to_one_file_in_a_batch_do_not_lose_each_other() {
        let dir = project_dir("edit-lock");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("note.txt"), "foo").unwrap();

        let registry = ToolRegistry::with_builtin_tools(&dir).unwrap();
        let calls = vec![
            edit_call("call-1", "note.txt", "foo", "bar"),
            edit_call("call-2", "sub/../note.txt", "foo", "baz"),
        ];

        let results = run_batch(registry, &calls, &dir).await;

        let errors = results.iter().filter(|result| result.is_error).count();
        assert_eq!(errors, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}

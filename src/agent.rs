pub mod messages;
pub mod metrics;
pub mod session;
mod spinner;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::output;
use crate::status::TurnStatus;
use crate::{
    config::Config,
    models::ModelRegistry,
    providers::{ProviderRequest, is_context_overflow},
    tools::{ToolRegistry, ToolResult, resolve_path},
};

use spinner::SpinnerGuard;

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

/// The most salient argument of a tool call for display — the command or
/// path. `Some("?")` when the tool is known but the argument is missing;
/// `None` when the tool itself is unknown.
fn key_argument<'a>(name: &str, arguments: &'a serde_json::Value) -> Option<&'a str> {
    let field = match name {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        _ => return None,
    };
    Some(arguments.get(field).and_then(|v| v.as_str()).unwrap_or("?"))
}

/// One line, always: a multi-line argument would otherwise scroll the
/// spinner's line away mid-run.
fn first_line(argument: &str) -> String {
    match argument.split_once('\n') {
        Some((first, _)) => format!("{} …", first.trim_end()),
        None => argument.to_string(),
    }
}

fn format_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    match key_argument(name, arguments) {
        Some(argument) => format!("{name}: {}", first_line(argument)),
        None => name.to_string(),
    }
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
fn file_key(cwd: &Path, name: &str, arguments: &serde_json::Value) -> Option<PathBuf> {
    let field = match name {
        "read" | "write" | "edit" => "path",
        _ => return None,
    };
    let raw = arguments.get(field)?.as_str()?;
    Some(normalize(&resolve_path(cwd, raw)))
}

/// Collapse `.` and `..` lexically, without touching the filesystem, so two
/// spellings of one path share a key — `sub/../note.txt` and `note.txt` must
/// not look like two different files to the lock.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
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
                break;
            }

            self.execute_tool_calls(response.tool_calls).await?;
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

        let results = run_batch(self.tool_registry.clone(), &tool_calls, &self.config.cwd).await;

        spinner.stop();

        self.metrics.log(
            "tool_exec",
            json!({
                "duration_ms": tool_start.elapsed().as_millis(),
                "tools": tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
                "success": results.iter().all(|r| !r.is_error),
            }),
        );

        for (tc, result) in tool_calls.iter().zip(results) {
            let tool_label = format_tool_call(&tc.name, &tc.arguments);

            self.session.push_tool_result(
                tc.id.clone(),
                tc.name.clone(),
                result.content.clone(),
                result.is_error,
            )?;

            if result.is_error {
                output::fail(&format!("{tool_label} (failed: {})", result.content));
            } else {
                let lines = result.content.lines().count();
                let bytes = result.content.len();
                output::tick(&format!("{tool_label} ({lines} lines, {bytes} bytes)"));
            }
        }

        Ok(())
    }

    pub fn summary(&self) -> String {
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
        let session_path = self.session.path().display();

        format!(
            "model    : {}\n\
             available: {}\n\
             tools    : {tools}\n\
             session  : {session_path} ({session_state})\n\
             context  : {context}",
            self.models.active(),
            self.models.by_provider()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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

pub mod messages;
pub mod metrics;
pub mod session;

mod spinner;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;

use crate::output;
use crate::{
    config::Config,
    models::ModelRegistry,
    providers::{ProviderRequest, is_context_overflow},
    tools::{ToolRegistry, ToolResult},
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

pub struct Agent {
    config: Config,
    models: Arc<ModelRegistry>,
    tool_registry: ToolRegistry,
    session: session::Session,
    metrics: metrics::Metrics,
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
        })
    }

    pub fn models(&self) -> &ModelRegistry {
        &self.models
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
        output::step(&format!(
            "cleared context, new session: {}",
            self.session.path().display()
        ));
        Ok(())
    }

    pub async fn run_once(&mut self, prompt: String) -> Result<()> {
        self.session.push_user(prompt)?;

        let max_iterations: usize = crate::config::env_or(
            "ONELOOP_MAX_ITERATIONS",
            crate::config::DEFAULT_MAX_ITERATIONS,
        );

        // None until a request completes; a fallback answer pins the loop to
        // whichever model actually replied.
        let mut active_model: Option<String> = None;

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
                    active_model = Some(used_model);
                    response
                }
                Err(e) => {
                    spinner.stop();
                    self.log_api_call(&requested_model, api_start, tokens_estimated, false);
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
            self.log_api_call(&used_model, api_start, tokens_estimated, true);

            if !response.content.trim().is_empty() {
                self.session.push_assistant(response.content.clone())?;
                println!("{}", response.content);
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

    fn log_api_call(&self, model: &str, started: Instant, tokens_estimated: usize, success: bool) {
        self.metrics.log(
            "api_call",
            json!({
                "model": model,
                // A log line is not worth failing over an unknown name.
                "model_id": self.models.get(model)
                    .map_or_else(|_| "?".to_string(), |m| m.id.clone()),
                "duration_ms": started.elapsed().as_millis(),
                "tokens_estimated": tokens_estimated,
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

        let handles: Vec<_> = tool_calls
            .iter()
            .map(|tc| {
                let name = tc.name.clone();
                let arguments = tc.arguments.clone();
                // A call whose arguments never decoded cannot be run, but it
                // can be answered. Reported as a tool failure it reaches the
                // model as a result it can act on, instead of ending the run.
                let parse_error = tc.parse_error.clone();
                let ctx = AgentContext {
                    cwd: self.config.cwd.clone(),
                };
                let registry = self.tool_registry.clone();
                tokio::spawn(async move {
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

        let results: Vec<_> = join_all(handles)
            .await
            .into_iter()
            .map(|res| match res {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => ToolResult {
                    content: format!("Tool execution failed: {e:#}"),
                    is_error: true,
                },
                Err(join_err) => ToolResult {
                    content: format!("Tool task failed: {join_err}"),
                    is_error: true,
                },
            })
            .collect();

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

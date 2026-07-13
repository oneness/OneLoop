pub mod compaction;
pub mod evidence;
pub mod messages;
pub mod metrics;
pub mod orchestration;
pub mod session;

mod spinner;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;

use crate::{
    config::Config,
    directives::ToolMode,
    providers::{ProviderRegistry, ProviderRequest},
    tools::{ToolRegistry, ToolResult},
};
use crate::output::{DIM, RED, RESET};

use spinner::SpinnerGuard;

/// Context passed to tool executions.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

/// The most salient argument of a tool call for display — the command, path,
/// or query. `Some("?")` when the tool is known but the argument is missing;
/// `None` when the tool itself is unknown.
fn key_argument<'a>(name: &str, arguments: &'a serde_json::Value) -> Option<&'a str> {
    let field = match name {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "web_search" => "query",
        _ => return None,
    };
    Some(arguments.get(field).and_then(|v| v.as_str()).unwrap_or("?"))
}

fn format_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    match key_argument(name, arguments) {
        Some(argument) => format!("{name}: {argument}"),
        None => name.to_string(),
    }
}

pub struct Agent {
    config: Config,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: ToolRegistry,
    session: session::Session,
    metrics: metrics::Metrics,
}

impl Agent {
    pub fn new(
        config: Config,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
    ) -> Result<Self> {
        let mut session = session::Session::open_or_create(&config.cwd)?;
        // A previous process may have been killed mid-run, leaving tool
        // calls without results that providers would reject.
        let repaired = session.repair_dangling_tool_calls()?;
        if repaired > 0 {
            println!(
                "{DIM}  → closed {repaired} interrupted tool call(s) from a previous run{RESET}"
            );
        }
        let metrics = metrics::Metrics::from_session_path(session.path())?;
        Ok(Self {
            config,
            provider_registry: Arc::new(provider_registry),
            tool_registry,
            session,
            metrics,
        })
    }

    /// Close out tool calls whose results were lost when a run was
    /// cancelled, so the next request isn't rejected by the provider.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<()> {
        let repaired = self.session.repair_dangling_tool_calls()?;
        if repaired > 0 {
            println!("{DIM}  → closed {repaired} interrupted tool call(s){RESET}");
        }
        Ok(())
    }

    /// Clear the session — rotates to a new empty session file.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::from_session_path(self.session.path())?;
        println!(
            "{DIM}  → cleared context, new session: {}{RESET}",
            self.session.path().display()
        );
        Ok(())
    }

    /// Check if auto-compaction is needed and perform it.
    pub async fn auto_compact_if_needed(&mut self, provider_override: Option<&str>) -> Result<()> {
        compaction::auto_compact_if_needed(self, provider_override).await
    }

    pub async fn run_consensus(
        &mut self,
        prompt: String,
        providers: Vec<String>,
        judge: Option<String>,
        tools: ToolMode,
    ) -> Result<()> {
        let mut ctx = orchestration::OrchestrationCtx {
            provider_registry: &self.provider_registry,
            tool_registry: &self.tool_registry,
            system_prompt: &self.config.system_prompt,
            cwd: &self.config.cwd,
            session: &mut self.session,
        };
        orchestration::run_consensus(&mut ctx, &prompt, &providers, &judge, &tools).await
    }

    pub async fn run_debate(
        &mut self,
        prompt: String,
        providers: Vec<String>,
        judge: Option<String>,
        rounds: usize,
        tools: ToolMode,
    ) -> Result<()> {
        let mut ctx = orchestration::OrchestrationCtx {
            provider_registry: &self.provider_registry,
            tool_registry: &self.tool_registry,
            system_prompt: &self.config.system_prompt,
            cwd: &self.config.cwd,
            session: &mut self.session,
        };
        orchestration::run_debate(&mut ctx, &prompt, &providers, &judge, rounds, &tools).await
    }

    pub async fn run_once_with(
        &mut self,
        prompt: String,
        provider_override: Option<&str>,
        model_override: Option<String>,
    ) -> Result<()> {
        self.session.push_user(prompt)?;

        let max_iterations: usize = crate::config::env_or(
            "ONELOOP_MAX_ITERATIONS",
            crate::config::DEFAULT_MAX_ITERATIONS,
        );

        let mut active_provider = provider_override.map(String::from);

        for _iteration in 1..=max_iterations {
            let spinner = SpinnerGuard::new("thinking...");
            let tokens_estimated = compaction::estimate_tokens(
                self.session.messages(),
                self.config
                    .system_prompt
                    .as_ref()
                    .map(String::len)
                    .unwrap_or(0),
            );
            let api_start = Instant::now();
            // For metrics: the provider this request is aimed at, which the
            // registry default only approximates when an override is active.
            let requested_provider = active_provider
                .clone()
                .unwrap_or_else(|| self.provider_registry.active_name().to_string());
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
                model_override: model_override.clone(),
            };

            let response = match self
                .provider_registry
                .complete_with_retry(
                    active_provider.as_deref(),
                    request,
                    Some(spinner.stop_callback()),
                    Some(spinner.start_callback("thinking...")),
                )
                .await
            {
                Ok((used_provider, response)) => {
                    active_provider = Some(used_provider);
                    response
                }
                Err(e) => {
                    spinner.stop();
                    self.metrics.log(
                        "api_call",
                        json!({
                            "provider": &requested_provider,
                            "model": model_override
                                .clone()
                                .unwrap_or_else(|| self.provider_registry.model_for(&requested_provider)),
                            "duration_ms": api_start.elapsed().as_millis(),
                            "tokens_estimated": tokens_estimated,
                            "success": false,
                        }),
                    );
                    println!("{RED}  ✗ provider error: {e:#}{RESET}");
                    break;
                }
            };
            spinner.stop();

            // active_provider was just set to the provider that answered
            // (it may differ from the requested one after a fallback).
            let used_provider = active_provider.clone().unwrap_or(requested_provider);
            self.metrics.log(
                "api_call",
                json!({
                    "provider": &used_provider,
                    "model": model_override
                        .clone()
                        .unwrap_or_else(|| self.provider_registry.model_for(&used_provider)),
                    "duration_ms": api_start.elapsed().as_millis(),
                    "tokens_estimated": tokens_estimated,
                    "success": true,
                }),
            );

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

    /// Record, execute in parallel, and report one batch of tool calls.
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
                let ctx = AgentContext {
                    cwd: self.config.cwd.clone(),
                };
                let registry = self.tool_registry.clone();
                tokio::spawn(async move { registry.execute(&name, arguments, &ctx).await })
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
                println!("{RED}  ✗ {tool_label}{RESET}");
                println!("{}", result.content);
            } else {
                let lines = result.content.lines().count();
                let bytes = result.content.len();
                println!("{DIM}  ✓ {tool_label} ({lines} lines, {bytes} bytes){RESET}");
            }
        }

        Ok(())
    }

    /// One-line provider identification, e.g. `openrouter (deepseek/deepseek-v4-flash)`.
    pub fn provider_line(&self) -> String {
        format!(
            "{} ({})",
            self.provider_registry.active_name(),
            self.provider_registry.active_model()
        )
    }

    pub fn summary(&self) -> String {
        let message_count = self.session.messages().len();
        let provider = self.provider_registry.active_name();
        let provider_model = self.provider_registry.active_model();
        let all_providers = self.provider_registry.available_providers().join(", ");
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
            "provider : {provider} ({provider_model})\n\
             available: {all_providers}\n\
             tools    : {tools}\n\
             session  : {session_path} ({session_state})\n\
             context  : {context}"
        )
    }
}

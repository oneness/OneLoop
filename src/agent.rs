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
use std::env;
use std::sync::Arc;

use crate::{
    config::Config,
    directives::ToolMode,
    providers::{ProviderRegistry, ProviderRequest},
    tools::{ToolRegistry, ToolResult},
};

use spinner::SpinnerGuard;

/// Context passed to tool executions.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

fn format_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    match name {
        "bash" => {
            let cmd = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("bash: {cmd}")
        }
        "read" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("read: {path}")
        }
        "write" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("write: {path}")
        }
        "edit" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("edit: {path}")
        }
        "web_search" => {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("web_search: {query}")
        }
        _ => name.to_string(),
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
        let session = session::Session::open_or_create(&config.cwd)?;
        let metrics = metrics::Metrics::from_session_path(session.path())?;
        Ok(Self {
            config,
            provider_registry: Arc::new(provider_registry),
            tool_registry,
            session,
            metrics,
        })
    }

    /// Clear the session — rotates to a new empty session file.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::from_session_path(self.session.path())?;
        println!(
            "\x1b[90m  → cleared context, new session: {}\x1b[0m",
            self.session.path().display()
        );
        Ok(())
    }

    /// Check if auto-compaction is needed and perform it.
    pub async fn auto_compact_if_needed(&mut self, provider_override: Option<&str>) -> Result<()> {
        let system_prompt_chars = self
            .config
            .system_prompt
            .as_ref()
            .map(String::len)
            .unwrap_or(0);

        if !compaction::should_compact(self.session.messages(), system_prompt_chars) {
            return Ok(());
        }

        println!("\x1b[33m  ⚠ context near limit — auto-compacting...\x1b[0m");

        let tokens_before =
            compaction::estimate_tokens(self.session.messages(), system_prompt_chars);
        let compact_start = Instant::now();

        let lightweight = compaction::strip_tool_outputs(self.session.messages());

        let spinner = SpinnerGuard::new("compacting...");

        use crate::agent::messages::{Message, UserMessage};
        let mut compact_messages = lightweight;
        compact_messages.push(Message::User(UserMessage {
            content: compaction::compaction_user_message(),
        }));

        let request = ProviderRequest {
            system_prompt: self.config.system_prompt.clone(),
            messages: compact_messages,
            tools: Vec::new(),
            model_override: None,
        };

        let response = match self
            .provider_registry
            .complete_with_retry(
                provider_override,
                request,
                Some(spinner.stop_callback()),
                Some(spinner.start_callback("compacting...")),
            )
            .await
        {
            Ok((_used_provider, response)) => response,
            Err(e) => {
                spinner.stop();
                eprintln!("\x1b[31m  ✗ compaction failed: {e:#}\x1b[0m");
                return Ok(());
            }
        };
        spinner.stop();

        let summary = response.content;

        let recent_user_messages =
            compaction::collect_recent_user_messages(self.session.messages());

        self.session = self.session.rotate()?;
        self.metrics = metrics::Metrics::from_session_path(self.session.path())?;

        for user_msg in &recent_user_messages {
            self.session.push_user(user_msg.clone())?;
        }

        self.session
            .push_user(format!("{}{}", compaction::SUMMARY_PREFIX, summary))?;
        self.session.push_assistant(
            "Understood. I have the context from the previous session. Ready to continue."
                .to_string(),
        )?;

        let tokens_after =
            compaction::estimate_tokens(self.session.messages(), system_prompt_chars);
        let compact_duration = compact_start.elapsed();

        self.metrics.log(
            "compaction",
            json!({
                "duration_ms": compact_duration.as_millis(),
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
            }),
        );

        println!(
            "\x1b[32m  ✓ compacted — new session: {} ({} recent messages preserved)\x1b[0m",
            self.session.path().display(),
            recent_user_messages.len()
        );
        println!(
            "\x1b[90m  ⚠ long threads and multiple compactions can reduce accuracy. use /clear when possible to keep sessions focused.\x1b[0m"
        );

        Ok(())
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

        let max_iterations: usize = env::var("ONELOOP_MAX_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);

        let mut active_provider = provider_override.map(String::from);

        for _iteration in 1..=max_iterations {
            if crate::app::is_stop_requested() {
                println!("\x1b[33m  ⏹ stopped\x1b[0m");
                return Ok(());
            }

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
                            "provider": self.provider_registry.active_name(),
                            "model": self.provider_registry.active_model(),
                            "duration_ms": api_start.elapsed().as_millis(),
                            "tokens_estimated": tokens_estimated,
                            "success": false,
                        }),
                    );
                    println!("\x1b[31m  ✗ provider error: {e:#}\x1b[0m");
                    break;
                }
            };
            spinner.stop();

            self.metrics.log(
                "api_call",
                json!({
                    "provider": self.provider_registry.active_name(),
                    "model": self.provider_registry.active_model(),
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

            let tool_calls = response.tool_calls;
            let tool_start = Instant::now();

            for tc in &tool_calls {
                self.session.push_tool_call(
                    tc.id.clone(),
                    tc.name.clone(),
                    tc.arguments.clone(),
                )?;
            }

            let tool_spinner = SpinnerGuard::new("running tools...");

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

            tool_spinner.stop();

            let tool_duration = tool_start.elapsed();

            self.metrics.log(
                "tool_exec",
                json!({
                    "duration_ms": tool_duration.as_millis(),
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
                    println!("\x1b[31m  ✗ {tool_label}\x1b[0m");
                    println!("{}", result.content);
                } else {
                    let lines = result.content.lines().count();
                    let bytes = result.content.len();
                    println!("\x1b[90m  ✓ {tool_label} ({lines} lines, {bytes} bytes)\x1b[0m");
                }
            }
        }

        Ok(())
    }

    pub fn summary(&self) -> String {
        let message_count = self.session.messages().len();
        let provider = self.provider_registry.active_name();
        let provider_model = self.provider_registry.active_model();
        let all_providers = self.provider_registry.available_providers().join(", ");
        let tools = self.tool_registry.names().join(", ");
        let has_system = self
            .config
            .system_prompt
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());

        let session_info = if message_count > 0 {
            format!(
                "session: {} ({message_count} messages)",
                self.session.path().display()
            )
        } else {
            format!("session: {} (new)", self.session.path().display())
        };

        format!(
            "provider: {provider} ({provider_model})\n\
             available: {all_providers}\n\
             tools: {tools}\n\
             {session_info}\n\
             system_prompt: {}",
            if has_system { "loaded" } else { "none" }
        )
    }
}

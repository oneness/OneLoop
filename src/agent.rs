pub mod compaction;
pub mod messages;
pub mod metrics;
pub mod session;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::json;
use std::sync::Arc;
use std::env;

use crate::{
    config::Config,
    directives::ToolMode,
    providers::{ProviderRegistry, ProviderRequest},
    tools::{ToolRegistry, ToolResult},
};

/// Context passed to tool executions.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// RAII guard for a spinner task. Aborts the spinner and clears the line on drop.
struct SpinnerGuard {
    handle: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SpinnerGuard {
    fn new(label: &str) -> Self {
        let label = label.to_string();
        let handle = tokio::spawn(async move {
            let mut i = 0;
            loop {
                let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
                eprint!("\x1b[2K\r\x1b[90m  {frame} {label}\x1b[0m\r");
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                i += 1;
            }
        });
        Self {
            handle: std::sync::Arc::new(std::sync::Mutex::new(Some(handle))),
        }
    }

    fn stop(&self) {
        if let Ok(mut handle) = self.handle.lock()
            && let Some(handle) = handle.take()
        {
            handle.abort();
            eprint!("\x1b[2K\r");
        }
    }

    /// Returns a callback that stops the spinner, suitable for passing to
    /// `complete_with_retry` so it can halt the animation before showing an
    /// interactive prompt.
    fn stop_callback(self: &SpinnerGuard) -> Box<dyn FnOnce() + Send> {
        let handle = self.handle.clone();
        Box::new(move || {
            if let Ok(mut handle) = handle.lock()
                && let Some(h) = handle.take()
            {
                h.abort();
                eprint!("\x1b[2K\r");
            }
        })
    }

    /// Returns a callback that starts a new spinner, used after an interactive
    /// prompt to resume the animation.
    fn start_callback(self: &SpinnerGuard, label: &str) -> Box<dyn FnOnce() + Send> {
        let handle = self.handle.clone();
        let label = label.to_string();
        Box::new(move || {
            let new_handle = tokio::spawn(async move {
                let mut i = 0;
                loop {
                    let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
                    eprint!("\x1b[2K\r\x1b[90m  {frame} {label}\x1b[0m\r");
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    i += 1;
                }
            });
            if let Ok(mut handle) = handle.lock() {
                *handle = Some(new_handle);
            }
        })
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.stop();
    }
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

fn format_labeled_responses(responses: &[(String, String)]) -> String {
    responses
        .iter()
        .map(|(provider, content)| format!("── {provider} ──\n{}", content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_synthesis_prompt(prompt: &str, responses: &[(String, String)], label: &str) -> String {
    format!(
        "The user asked:\n\n{prompt}\n\nSeveral models answered independently:\n\n{}\n\nSynthesize a final {label}. Identify agreements, disagreements, tradeoffs, and a practical recommendation. Do not simply average the answers; prefer the best-supported reasoning.",
        format_labeled_responses(responses)
    )
}

fn format_debate_round_prompt(
    prompt: &str,
    transcript: &[(String, String)],
    round: usize,
) -> String {
    format!(
        "The user asked:\n\n{prompt}\n\nDebate transcript so far:\n\n{}\n\nThis is critique/revision round {round}. Critique the other responses, identify where your previous reasoning may be incomplete, and provide a revised position.",
        format_labeled_responses(transcript)
    )
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
    /// Called after each agent loop completes. If the context is over threshold,
    /// it calls the provider to generate a summary, rotates the session, and
    /// injects the summary as the initial context.
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

        // Build a lightweight version of the conversation for the compaction call.
        // Tool results can be huge (file contents, command output) — replace them
        // with one-line summaries so the compaction LLM call is fast.
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

        // Preserve recent user messages so the next session has verbatim context.
        // Must collect before rotating since rotate() creates an empty session.
        let recent_user_messages =
            compaction::collect_recent_user_messages(self.session.messages());

        // Rotate to a new session with the summary.
        self.session = self.session.rotate()?;

        // Update metrics to point to new session file.
        self.metrics = metrics::Metrics::from_session_path(self.session.path())?;

        // Replay recent user messages verbatim.
        for user_msg in &recent_user_messages {
            self.session.push_user(user_msg.clone())?;
        }

        // Inject the structured summary as the final user message.
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
        providers
            .iter()
            .try_for_each(|provider| self.provider_registry.validate_provider(provider))?;
        if let Some(judge) = &judge {
            self.provider_registry.validate_provider(judge)?;
        }
        self.validate_orchestration_tools(&tools)?;
        self.session.push_user(prompt.clone())?;

        let responses = self
            .collect_provider_responses(&providers, &prompt, "consensus", &tools)
            .await?;
        let initial_output = format_labeled_responses(&responses);
        println!("{initial_output}");
        self.session.push_assistant(initial_output)?;

        let judge = judge.unwrap_or_else(|| providers[0].clone());
        let synthesis = self
            .synthesize_consensus(&judge, &prompt, &responses, "Consensus")
            .await?;
        let output = format!("── Consensus ({judge}) ──\n{synthesis}");
        println!("\n{output}");
        self.session.push_assistant(output)?;
        Ok(())
    }

    pub async fn run_debate(
        &mut self,
        prompt: String,
        providers: Vec<String>,
        judge: Option<String>,
        rounds: usize,
        tools: ToolMode,
    ) -> Result<()> {
        providers
            .iter()
            .try_for_each(|provider| self.provider_registry.validate_provider(provider))?;
        if let Some(judge) = &judge {
            self.provider_registry.validate_provider(judge)?;
        }
        self.validate_orchestration_tools(&tools)?;
        self.session.push_user(prompt.clone())?;

        let mut transcript = self
            .collect_provider_responses(&providers, &prompt, "initial answer", &tools)
            .await?;
        let mut output = format!(
            "── Round 1: Initial Answers ──\n\n{}",
            format_labeled_responses(&transcript)
        );
        println!("{output}");

        for round in 1..=rounds {
            let debate_prompt = format_debate_round_prompt(&prompt, &transcript, round);
            let critiques = self
                .collect_provider_responses(&providers, &debate_prompt, "critique/revision", &tools)
                .await?;
            let section = format!(
                "── Round {}: Critiques/Revisions ──\n\n{}",
                round + 1,
                format_labeled_responses(&critiques)
            );
            println!("\n{section}");
            output.push_str("\n\n");
            output.push_str(&section);
            transcript.extend(critiques);
        }

        self.session.push_assistant(output)?;

        let judge = judge.unwrap_or_else(|| providers[0].clone());
        let synthesis = self
            .synthesize_consensus(&judge, &prompt, &transcript, "Final Consensus")
            .await?;
        let output = format!("── Final Consensus ({judge}) ──\n{synthesis}");
        println!("\n{output}");
        self.session.push_assistant(output)?;
        Ok(())
    }

    fn validate_orchestration_tools(&self, tools: &ToolMode) -> Result<()> {
        match tools {
            ToolMode::Default | ToolMode::None => Ok(()),
            ToolMode::AllowList(names) => {
                let available = self.tool_registry.names();
                let unsupported: Vec<&str> = names
                    .iter()
                    .map(String::as_str)
                    .filter(|name| !matches!(*name, "read" | "web_search"))
                    .collect();
                if !unsupported.is_empty() {
                    bail!(
                        "only read-only tools allowed in multi-model orchestration: {}",
                        unsupported.join(", ")
                    );
                }
                let unknown: Vec<&str> = names
                    .iter()
                    .map(String::as_str)
                    .filter(|name| !available.contains(name))
                    .collect();
                if !unknown.is_empty() {
                    bail!("unknown tools: {}", unknown.join(", "));
                }
                Ok(())
            }
        }
    }

    /// Resolve tool definitions for orchestration modes.
    /// Default = read + web_search. Explicit tools:none = empty.
    fn orchestration_tool_definitions(&self, tools: &ToolMode) -> Vec<crate::tools::ToolDefinition> {
        match tools {
            ToolMode::Default => self.tool_registry.definitions_for(&[
                "read".to_string(),
                "web_search".to_string(),
            ]),
            ToolMode::None => Vec::new(),
            ToolMode::AllowList(names) => self.tool_registry.definitions_for(names),
        }
    }

    async fn collect_provider_responses(
        &self,
        providers: &[String],
        prompt: &str,
        purpose: &str,
        tools: &ToolMode,
    ) -> Result<Vec<(String, String)>> {
        let tool_definitions = self.orchestration_tool_definitions(tools);
        let max_tool_iterations: usize = env::var("ONELOOP_ORCHESTRATION_MAX_TOOL_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let spinner = SpinnerGuard::new(&format!("multi-model {purpose}..."));
        let provider_registry = self.provider_registry.clone();
        let tool_registry = self.tool_registry.clone();
        let system_prompt = self.config.system_prompt.clone();
        let cwd = self.config.cwd.clone();

        let handles: Vec<_> = providers
            .iter()
            .map(|provider_name| {
                let provider_name = provider_name.clone();
                let provider_registry = provider_registry.clone();
                let tool_registry = tool_registry.clone();
                let system_prompt = system_prompt.clone();
                let cwd = cwd.clone();
                let tool_definitions = tool_definitions.clone();
                let prompt_text = prompt.to_string();
                let max_iterations = max_tool_iterations;

                tokio::spawn(async move {
                    let mut req = ProviderRequest {
                        system_prompt,
                        messages: vec![messages::Message::User(messages::UserMessage {
                            content: prompt_text,
                        })],
                        tools: tool_definitions,
                    };
                    let provider_label = provider_name.clone();

                    for iteration in 0..max_iterations {
                        let response = provider_registry
                            .complete_once(&provider_name, req.clone())
                            .await?;

                        // No tool calls → final answer.
                        if response.tool_calls.is_empty() {
                            return Ok::<_, anyhow::Error>((provider_label, response.content));
                        }

                        // Execute tool calls in parallel.
                        let tool_results: Vec<ToolResult> = {
                            let handles: Vec<_> = response
                                .tool_calls
                                .iter()
                                .map(|tc| {
                                    let name = tc.name.clone();
                                    let arguments = tc.arguments.clone();
                                    let ctx = AgentContext { cwd: cwd.clone() };
                                    let registry = tool_registry.clone();
                                    tokio::spawn(async move {
                                        registry.execute(&name, arguments, &ctx).await
                                    })
                                })
                                .collect();

                            join_all(handles)
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
                                .collect()
                        };

                        // Print tool activity.
                        for (tc, result) in response.tool_calls.iter().zip(&tool_results) {
                            let label = format_tool_call(&tc.name, &tc.arguments);
                            if result.is_error {
                                eprintln!("\x1b[90m    {provider_label} ✗ {label}\x1b[0m");
                            } else {
                                let lines = result.content.lines().count();
                                let bytes = result.content.len();
                                eprintln!(
                                    "\x1b[90m    {provider_label} ✓ {label} ({lines} lines, {bytes} bytes)\x1b[0m"
                                );
                            }
                        }

                        // Append tool results to conversation for next iteration.
                        use crate::agent::messages::{
                            AssistantMessage, Message, ToolCall as MsgToolCall, ToolResultMessage,
                        };
                        req.messages.push(Message::Assistant(AssistantMessage {
                            content: response.content.clone(),
                        }));
                        for tc in &response.tool_calls {
                            req.messages.push(Message::ToolCall(MsgToolCall {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            }));
                        }
                        for (tc, result) in response.tool_calls.iter().zip(tool_results) {
                            req.messages.push(Message::ToolResult(ToolResultMessage {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                content: result.content,
                                is_error: result.is_error,
                            }));
                        }

                        // Last iteration — return whatever content we have.
                        if iteration == max_iterations - 1 {
                            return Ok((provider_label, response.content));
                        }
                    }

                    Ok((provider_label, String::new()))
                })
            })
            .collect();

        let results = join_all(handles).await;
        spinner.stop();
        // Flatten JoinError + inner error.
        results
            .into_iter()
            .map(|res| match res {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(e),
                Err(join_err) => bail!("provider task failed: {join_err}"),
            })
            .collect()
    }

    async fn synthesize_consensus(
        &self,
        judge: &str,
        prompt: &str,
        responses: &[(String, String)],
        label: &str,
    ) -> Result<String> {
        let content = format_synthesis_prompt(prompt, responses, label);
        let request = ProviderRequest {
            system_prompt: self.config.system_prompt.clone(),
            messages: vec![messages::Message::User(messages::UserMessage { content })],
            tools: Vec::new(),
        };

        let spinner = SpinnerGuard::new("synthesizing consensus...");
        let response = self.provider_registry.complete_once(judge, request).await;
        spinner.stop();
        response.map(|response| response.content)
    }

    pub async fn run_once_with(
        &mut self,
        prompt: String,
        provider_override: Option<&str>,
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
                    // Persist the provider for subsequent iterations.
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

            // Record tool calls in session, then execute in parallel.
            // Session records must be sequential (JSONL ordering), but execution
            // can happen concurrently — e.g. two `read` calls at the same time.
            let tool_calls = response.tool_calls;
            let tool_start = Instant::now();

            // Log all tool calls to session first.
            for tc in &tool_calls {
                self.session.push_tool_call(
                    tc.id.clone(),
                    tc.name.clone(),
                    tc.arguments.clone(),
                )?;
            }

            // Start a spinner while tools execute.
            let tool_spinner = SpinnerGuard::new("running tools...");

            // Spawn all tool executions as separate tasks.
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

            // Await all handles in spawn order — preserves original ordering.
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

            // Stop tool spinner before printing results.
            tool_spinner.stop();

            let tool_duration = tool_start.elapsed();

            // Log tool execution metrics.
            self.metrics.log(
                "tool_exec",
                json!({
                    "duration_ms": tool_duration.as_millis(),
                    "tools": tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
                    "success": results.iter().all(|r| !r.is_error),
                }),
            );

            // Now log results to session and print feedback in order.
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

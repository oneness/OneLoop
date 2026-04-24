pub mod messages;
pub mod session;

use std::path::PathBuf;

use anyhow::Result;
use std::env;

use crate::{
    config::Config,
    providers::{ProviderRegistry, ProviderRequest},
    tools::{ToolRegistry, ToolResult},
};

/// Context passed to tool executions.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

const SPINNER_FRAMES: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// RAII guard for a spinner task. Aborts the spinner and clears the line on drop.
struct SpinnerGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
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
        Self { handle: Some(handle) }
    }

    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            eprint!("\x1b[2K\r");
        }
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

pub struct Agent {
    config: Config,
    provider_registry: ProviderRegistry,
    tool_registry: ToolRegistry,
    session: session::Session,
}

impl Agent {
    pub fn new(
        config: Config,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
    ) -> Result<Self> {
        let session = session::Session::open_or_create(&config.cwd)?;
        Ok(Self {
            config,
            provider_registry,
            tool_registry,
            session,
        })
    }

    /// Clear the session — rotates to a new empty session file.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session = self.session.rotate()?;
        println!(
            "\x1b[90m  → cleared context, new session: {}\x1b[0m",
            self.session.path().display()
        );
        Ok(())
    }

    pub async fn run_once_with(
        &mut self,
        prompt: String,
        provider_override: Option<&str>,
    ) -> Result<()> {
        self.session.push_user(prompt)?;

        let ctx = AgentContext {
            cwd: self.config.cwd.clone(),
        };

        let max_iterations: usize = env::var("ONELOOP_MAX_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);

        for _iteration in 1..=max_iterations {
            if crate::app::is_stop_requested() {
                println!("\x1b[33m  ⏹ stopped\x1b[0m");
                return Ok(());
            }

            let mut spinner = SpinnerGuard::new("thinking...");
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
            };

            let response = match self
                .provider_registry
                .complete_with(provider_override, request)
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    spinner.stop();
                    println!("\x1b[31m  ✗ provider error: {e:#}\x1b[0m");
                    break;
                }
            };
            spinner.stop();

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

            for tool_call in response.tool_calls {
                self.session.push_tool_call(
                    tool_call.id.clone(),
                    tool_call.name.clone(),
                    tool_call.arguments.clone(),
                )?;

                let tool_label = format_tool_call(&tool_call.name, &tool_call.arguments);
                let mut spinner = SpinnerGuard::new(&tool_label);

                let result = match self
                    .tool_registry
                    .execute(&tool_call.name, tool_call.arguments.clone(), &ctx)
                    .await
                {
                    Ok(result) => result,
                    Err(e) => ToolResult {
                        content: format!("Tool execution failed: {e:#}"),
                        is_error: true,
                    },
                };

                spinner.stop();

                self.session.push_tool_result(
                    tool_call.id,
                    tool_call.name,
                    result.content.clone(),
                    result.is_error,
                )?;

                if result.is_error {
                    eprint!("\x1b[2K\r");
                    println!("\x1b[31m  ✗ {tool_label}\x1b[0m");
                    println!("{}", result.content);
                } else {
                    let lines = result.content.lines().count();
                    let bytes = result.content.len();
                    eprint!("\x1b[2K\r");
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
             system_prompt: {}\n\
             hint: prefix with @provider (e.g. @anthropic) to route to a specific provider",
            if has_system { "loaded" } else { "none" }
        )
    }
}

use std::io::{self, Write as IoWrite};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::{agent::Agent, config::Config, providers::ProviderRegistry, tools::ToolRegistry};

pub struct App {
    config: Config,
}

/// Shared flag to signal the agent loop to stop.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

pub fn clear_stop_requested() {
    STOP_REQUESTED.store(false, Ordering::Relaxed);
}

/// Parse interactive commands. Returns (command, rest_of_input) or None if it's not a command.
fn parse_command(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed == "/clear" {
        return Some("clear");
    }
    None
}

impl App {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self, prompt: Option<String>) -> Result<()> {
        let provider_registry = ProviderRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools();
        let mut agent = Agent::new(self.config, provider_registry, tool_registry)?;

        match prompt {
            Some(prompt) => {
                let (provider, prompt) = parse_provider_prefix(&prompt);
                agent.run_once_with(prompt, provider).await
            }
            None => {
                println!("oneloop");
                println!("{}", agent.summary());
                println!();
                println!("interactive mode — type your message, /clear to reset context, Ctrl+C to stop");
                println!();

                loop {
                    print!("> ");
                    io::stdout().flush()?;

                    let mut input = String::new();
                    match io::stdin().read_line(&mut input) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = input.trim().to_string();
                            if line.is_empty() {
                                continue;
                            }

                            // Check for built-in commands.
                            if let Some(cmd) = parse_command(&line) {
                                match cmd {
                                    "clear" => {
                                        agent.clear_session()?;
                                        println!();
                                        continue;
                                    }
                                    _ => {}
                                }
                            }

                            let (provider, prompt) = parse_provider_prefix(&line);

                            // Clear any previous stop flag and arm the Ctrl+C handler.
                            clear_stop_requested();

                            // Use select to race the agent run against Ctrl+C.
                            tokio::select! {
                                result = agent.run_once_with(prompt, provider) => {
                                    if let Err(e) = result {
                                        eprintln!("\x1b[31m  ✗ {e:#}\x1b[0m");
                                    }
                                }
                                _ = tokio::signal::ctrl_c() => {
                                    STOP_REQUESTED.store(true, Ordering::Relaxed);
                                    println!("\x1b[33m  ⏹ stopped\x1b[0m");
                                }
                            }

                            // Auto-compact if context is near limit.
                            agent.auto_compact_if_needed(provider).await?;

                            println!();
                        }
                    }
                }

                Ok(())
            }
        }
    }
}

/// Parse a `@provider` prefix from the beginning of a prompt.
/// Returns `(Some("anthropic"), "say hello")` for `"@anthropic say hello"`.
/// Returns `(None, "say hello")` for `"say hello"`.
fn parse_provider_prefix(input: &str) -> (Option<&str>, String) {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix('@')
        && let Some(space_pos) = rest.find(char::is_whitespace)
    {
        let provider = &rest[..space_pos];
        let prompt = rest[space_pos..].trim();
        if !provider.is_empty() && !prompt.is_empty() {
            return (Some(provider), prompt.to_string());
        }
    }
    (None, trimmed.to_string())
}

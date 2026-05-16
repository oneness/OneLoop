use std::io::{self, Write as IoWrite};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::{
    agent::Agent,
    config::Config,
    directives::{PromptDirectives, RunMode, parse_prompt},
    providers::ProviderRegistry,
    tools::ToolRegistry,
};

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

/// Parse interactive commands. Returns Some(command) or None if it's not a command.
fn parse_command(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed == "/clear" {
        return Some("clear");
    }
    None
}

fn print_directive_summary(directives: &PromptDirectives) {
    match &directives.mode {
        RunMode::Single {
            provider: Some(provider),
        } => {
            eprintln!("\x1b[90m  → provider: {provider}\x1b[0m");
        }
        RunMode::Single { provider: None } => {}
        RunMode::Consensus { providers } => {
            eprintln!("\x1b[90m  → consensus: {}\x1b[0m", providers.join(", "));
        }
        RunMode::Debate { providers } => {
            eprintln!("\x1b[90m  → debate: {}\x1b[0m", providers.join(", "));
        }
    }
}

async fn run_directives(agent: &mut Agent, directives: PromptDirectives) -> Result<()> {
    print_directive_summary(&directives);
    match directives.mode {
        RunMode::Single { provider } => {
            agent
                .run_once_with(directives.prompt, provider.as_deref())
                .await
        }
        RunMode::Consensus { providers } => {
            agent
                .run_consensus(
                    directives.prompt,
                    providers,
                    directives.judge,
                    directives.tools,
                )
                .await
        }
        RunMode::Debate { providers } => {
            agent
                .run_debate(
                    directives.prompt,
                    providers,
                    directives.judge,
                    directives.rounds,
                    directives.tools,
                )
                .await
        }
    }
}

async fn run_directed_prompt(agent: &mut Agent, input: &str) -> Result<()> {
    run_directives(agent, parse_prompt(input)?).await
}

fn provider_override(directives: &PromptDirectives) -> Option<&str> {
    match &directives.mode {
        RunMode::Single { provider } => provider.as_deref(),
        RunMode::Consensus { .. } | RunMode::Debate { .. } => None,
    }
}

impl App {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self, prompt: Option<String>) -> Result<()> {
        let provider_registry = ProviderRegistry::new()?;
        let tool_registry = ToolRegistry::with_builtin_tools()?;
        let mut agent = Agent::new(self.config, provider_registry, tool_registry)?;

        match prompt {
            Some(prompt) => run_directed_prompt(&mut agent, &prompt).await,
            None => {
                println!("OneLoop");
                println!("{}", agent.summary());
                println!();
                println!(
                    "interactive mode — type your message, /clear to reset context, Ctrl+C to stop"
                );
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
                            if let Some("clear") = parse_command(&line) {
                                agent.clear_session()?;
                                println!();
                                continue;
                            }

                            let directives = match parse_prompt(&line) {
                                Ok(directives) => directives,
                                Err(e) => {
                                    eprintln!("\x1b[31m  ✗ {e:#}\x1b[0m");
                                    println!(
                                        "\x1b[90m  hint: use #!directive words#! <your message>, e.g. #!anthropic#! explain this file\x1b[0m"
                                    );
                                    println!();
                                    continue;
                                }
                            };
                            let compact_provider_override =
                                provider_override(&directives).map(String::from);

                            // Clear any previous stop flag and arm the Ctrl+C handler.
                            clear_stop_requested();

                            // Use select to race the agent run against Ctrl+C.
                            tokio::select! {
                                result = run_directives(&mut agent, directives) => {
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
                            agent
                                .auto_compact_if_needed(compact_provider_override.as_deref())
                                .await?;

                            println!();
                        }
                    }
                }

                Ok(())
            }
        }
    }
}

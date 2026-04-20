use std::io::{self, Write as IoWrite};

use anyhow::Result;

use crate::{agent::Agent, config::Config, providers::ProviderRegistry, tools::ToolRegistry};

pub struct App {
    config: Config,
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
                println!("interactive mode — type your message, Ctrl+D to exit");
                println!("prefix with @provider (e.g. @anthropic) to route to a specific provider");
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
                            let (provider, prompt) = parse_provider_prefix(&line);
                            agent.run_once_with(prompt, provider).await?;
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

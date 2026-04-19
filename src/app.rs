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
            Some(prompt) => agent.run_once(prompt).await,
            None => {
                println!("oneloop");
                println!("{}", agent.summary());
                println!();
                println!("interactive mode — type your message, Ctrl+D to exit");
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
                            agent.run_once(line).await?;
                            println!();
                        }
                    }
                }

                Ok(())
            }
        }
    }
}

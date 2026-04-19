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
                let prompt_templates = crate::ext::prompts::discover_prompt_templates();
                let skills = crate::ext::skills::discover_skills();

                println!("oneloop");
                println!();
                println!("{}", agent.summary());
                println!("prompt_templates: {}", prompt_templates.len());
                println!("skills: {}", skills.len());
                println!();
                println!("usage:");
                println!("  oneloop login anthropic");
                println!("  oneloop \"your prompt\"");
                println!("  oneloop \"read Cargo.toml\"");
                Ok(())
            }
        }
    }
}

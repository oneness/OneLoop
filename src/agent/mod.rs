pub mod context;
pub mod messages;
pub mod session;

use anyhow::Result;

use crate::{
    config::Config,
    providers::{ProviderRegistry, ProviderRequest},
    tools::ToolRegistry,
};

use self::{context::AgentContext, messages::Message};

pub struct Agent {
    config: Config,
    provider_registry: ProviderRegistry,
    tool_registry: ToolRegistry,
    session: session::Session,
}

impl Agent {
    pub fn new(config: Config, provider_registry: ProviderRegistry, tool_registry: ToolRegistry) -> Result<Self> {
        let session = session::Session::open_or_create(&config.cwd)?;
        Ok(Self {
            config,
            provider_registry,
            tool_registry,
            session,
        })
    }

    pub async fn run_once(&mut self, prompt: String) -> Result<()> {
        self.session.push_user(prompt)?;

        let ctx = AgentContext {
            cwd: self.config.cwd.clone(),
        };

        for _ in 0..8 {
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
            };

            let response = self.provider_registry.complete(request).await?;

            if !response.content.trim().is_empty() {
                self.session.push_assistant(response.content.clone())?;
                println!("{}", response.content);
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

                let result = self
                    .tool_registry
                    .execute(&tool_call.name, tool_call.arguments.clone(), &ctx)
                    .await?;

                self.session.push_tool_result(
                    tool_call.id,
                    tool_call.name,
                    result.content.clone(),
                    result.is_error,
                )?;

                println!("{}", result.content);
            }
        }

        Ok(())
    }

    pub fn summary(&self) -> String {
        let message_count = self.session.messages().len();
        let provider = self.provider_registry.active_name();
        let tools = self.tool_registry.names().join(", ");
        let tool_descriptions = self
            .tool_registry
            .descriptions()
            .into_iter()
            .map(|(name, description)| format!("  - {name}: {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        let has_system = self
            .config
            .system_prompt
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());
        let roles = self
            .session
            .messages()
            .iter()
            .map(|message| match message {
                Message::System(_) => "system",
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolCall(_) => "tool_call",
                Message::ToolResult(_) => "tool_result",
            })
            .collect::<Vec<_>>()
            .join(", ");

        format!(
            "cwd: {}\nsession: {}\nprovider: {}\ntools: {}\n{}\nsystem_prompt_loaded: {}\nmessage_count: {}\nmessage_roles: {}",
            self.config.cwd.display(),
            self.session.path().display(),
            provider,
            tools,
            tool_descriptions,
            has_system,
            message_count,
            roles,
        )
    }
}

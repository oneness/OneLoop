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

fn format_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    match name {
        "bash" => {
            let cmd = arguments.get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("bash: {}", cmd)
        }
        "read" => {
            let path = arguments.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("read: {}", path)
        }
        "write" => {
            let path = arguments.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("write: {}", path)
        }
        "edit" => {
            let path = arguments.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("edit: {}", path)
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
            eprint!("\x1b[90m...\x1b[0m\r");
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
            };

            let response = self.provider_registry.complete(request).await?;

            if !response.content.trim().is_empty() {
                self.session.push_assistant(response.content.clone())?;
                eprint!("\x1b[2K\r");
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

                let tool_label = format_tool_call(&tool_call.name, &tool_call.arguments);
                eprint!("\x1b[2K\r\x1b[90m  → {}\x1b[0m\r", tool_label);

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

                if result.is_error {
                    eprint!("\x1b[2K\r");
                    println!("\x1b[31m  ✗ {}\x1b[0m", tool_label);
                    println!("{}", result.content);
                } else {
                    let lines = result.content.lines().count();
                    let bytes = result.content.len();
                    eprint!("\x1b[2K\r");
                    println!("\x1b[90m  ✓ {} ({} lines, {} bytes)\x1b[0m", tool_label, lines, bytes);
                }
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

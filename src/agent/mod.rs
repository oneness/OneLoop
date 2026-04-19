pub mod context;
pub mod messages;
pub mod session;

use anyhow::Result;

use crate::{
    config::Config,
    providers::{ProviderRegistry, ProviderRequest},
    tools::ToolRegistry,
};

use self::context::AgentContext;

const SPINNER_FRAMES: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

fn start_spinner(label: &str) -> tokio::task::JoinHandle<()> {
    let label = label.to_string();
    tokio::spawn(async move {
        let mut i = 0;
        loop {
            let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
            eprint!("\x1b[2K\r\x1b[90m  {frame} {label}\x1b[0m\r");
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            i += 1;
        }
    })
}

fn stop_spinner(handle: tokio::task::JoinHandle<()>) {
    handle.abort();
    eprint!("\x1b[2K\r");
}

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
            let spinner = start_spinner("thinking...");
            let request = ProviderRequest {
                system_prompt: self.config.system_prompt.clone(),
                messages: self.session.messages().to_vec(),
                tools: self.tool_registry.definitions(),
            };

            let response = self.provider_registry.complete(request).await?;
            stop_spinner(spinner);

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

                let tool_label = format_tool_call(&tool_call.name, &tool_call.arguments);
                let spinner = start_spinner(&tool_label);

                let result = self
                    .tool_registry
                    .execute(&tool_call.name, tool_call.arguments.clone(), &ctx)
                    .await?;

                stop_spinner(spinner);

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
        let provider_model = self.provider_registry.active_model();
        let tools = self.tool_registry.names().join(", ");
        let has_system = self
            .config
            .system_prompt
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());

        let session_info = if message_count > 0 {
            format!("session: {} ({} messages)", self.session.path().display(), message_count)
        } else {
            format!("session: {} (new)", self.session.path().display())
        };

        format!(
            "provider: {provider} ({provider_model})\ntools: {tools}\n{session_info}\nsystem_prompt: {}",
            if has_system { "loaded" } else { "none" },
        )
    }
}

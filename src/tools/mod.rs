pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent::context::AgentContext;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_builtin_tools() -> Self {
        Self {
            tools: vec![
                Box::new(read::ReadTool),
                Box::new(write::WriteTool),
                Box::new(edit::EditTool),
                Box::new(bash::BashTool),
            ],
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name(),
                description: tool.description(),
                schema: tool.schema(),
            })
            .collect()
    }

    pub async fn execute(&self, name: &str, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            bail!("unknown tool: {name}");
        };

        tool.execute(input, ctx).await
    }
}

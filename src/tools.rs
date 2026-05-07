pub mod bash;
pub mod edit;
pub mod read;
pub mod web_search;
pub mod write;

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentContext;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_builtin_tools() -> Self {
        Self {
            tools: vec![
                Arc::new(read::ReadTool),
                Arc::new(write::WriteTool),
                Arc::new(edit::EditTool),
                Arc::new(bash::BashTool),
                Arc::new(web_search::WebSearchTool::new()),
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
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                schema: tool.schema(),
            })
            .collect()
    }

    pub fn definitions_for(&self, names: &[String]) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| names.iter().any(|name| name == tool.name()))
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                schema: tool.schema(),
            })
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: &AgentContext,
    ) -> Result<ToolResult> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            bail!("unknown tool: {name}");
        };
        tool.execute(input, ctx).await
    }
}

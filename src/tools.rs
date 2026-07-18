pub mod bash;
pub mod edit;
pub mod read;
pub mod skill;
pub mod write;

use std::path::Path;
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
    fn description(&self) -> String;
    fn schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_builtin_tools(cwd: &Path) -> Result<Self> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool),
            Arc::new(write::WriteTool),
            Arc::new(edit::EditTool),
            Arc::new(bash::BashTool),
        ];
        if let Some(skill_tool) = skill::SkillTool::new(cwd) {
            tools.push(Arc::new(skill_tool));
        }
        Ok(Self { tools })
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description(),
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
            // Models sometimes invent tool names from project instructions
            // (e.g. a CLI mentioned in AGENTS.md) — steer them back.
            bail!(
                "unknown tool: {name}. available tools: {}. \
                 CLI programs are run with the bash tool, not as tool calls.",
                self.names().join(", ")
            );
        };
        tool.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn unknown_tool_error_lists_available_tools() {
        let registry = ToolRegistry::with_builtin_tools(Path::new(".")).unwrap();
        let ctx = AgentContext {
            cwd: PathBuf::from("."),
        };

        let err = registry
            .execute("semble_search", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("unknown tool: semble_search"));
        assert!(msg.contains("read"));
        assert!(msg.contains("bash tool"));
    }
}

pub mod bash;
pub mod batch;
pub mod edit;
pub mod elisp;
pub mod read;
pub mod skill;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::messages::ToolDefinition;

/// What a tool is given besides its own arguments: where the agent works.
///
/// One field, and it stays one: a tool that needs more captures it when it is
/// constructed, rather than widening the world every other tool is handed.
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn schema(&self) -> Value;

    /// The body of this call's trace line, after `name: `. Rendered by the tool
    /// because only it knows how its argument should read — a command is
    /// elided, a path is resolved, a skill name is looked up on disk. `None`
    /// for a tool that names no single argument, which traces bare.
    ///
    /// A tool *with* a rule still gets here with the argument missing, which is
    /// why the builtins answer `Some("?")` rather than `None`: a known tool that
    /// forgot its argument should say so rather than look like one that has
    /// nothing to say.
    fn trace_label(&self, _arguments: &Value, _cwd: &Path) -> Option<String> {
        None
    }

    /// The argument that names one file, for the batch lock. `None` for a tool
    /// that cannot be reasoned about by path — bash and elisp can touch
    /// anything, so no single path stands for them.
    fn lock_path(&self) -> Option<&'static str> {
        None
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// A set given outright. Without this an agent can only ever have the
    /// builtin six, and a domain tool set is the whole point.
    pub fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn with_builtin_tools(cwd: &Path) -> Result<Self> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool),
            Arc::new(write::WriteTool),
            Arc::new(edit::EditTool),
            Arc::new(bash::BashTool),
            Arc::new(elisp::ElispTool),
        ];
        if let Some(skill_tool) = skill::SkillTool::new(cwd) {
            tools.push(Arc::new(skill_tool));
        }
        Ok(Self::from_tools(tools))
    }

    /// The tool answering to this name, for the two questions asked of it
    /// before it runs: how its call should trace, and which file it locks.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Arc::as_ref)
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

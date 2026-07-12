use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent::AgentContext,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head},
};

use super::{Tool, ToolResult};

pub struct ReadTool;

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> String {
        "Read file contents".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: ReadInput = serde_json::from_value(input)
            .context("invalid read input; expected { path: string }")?;

        let relative_path = input.path.trim_start_matches('@');
        let path = ctx.cwd.join(relative_path);

        if !tokio::fs::try_exists(&path)
            .await
            .with_context(|| format!("failed to check file existence: {}", path.display()))?
        {
            return Ok(ToolResult {
                content: format!(
                    "file not found: {}\nUse `bash` with `find` or `grep` to search for it, e.g.: find . -name \"*{}*\" -type f",
                    path.display(),
                    path.file_name()
                        .map(|f| f.to_string_lossy())
                        .unwrap_or_default()
                ),
                is_error: true,
            });
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read file: {}", path.display()))?;

        Ok(ToolResult {
            content: truncate_head(&content, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
            is_error: false,
        })
    }
}

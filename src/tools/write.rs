use std::{fs, path::Path};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::AgentContext;

use super::{Tool, ToolResult};

pub struct WriteTool;

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Write file contents"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Full file content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: WriteInput = serde_json::from_value(input)
            .context("invalid write input; expected { path: string, content: string }")?;

        let relative_path = input.path.trim_start_matches('@');
        let path = ctx.cwd.join(relative_path);

        if let Some(parent) = path.parent()
            && parent != Path::new("")
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directories for: {}", path.display()))?;
        }

        fs::write(&path, input.content)
            .with_context(|| format!("failed to write file: {}", path.display()))?;

        Ok(ToolResult {
            content: format!("wrote {}", path.display()),
            is_error: false,
        })
    }
}

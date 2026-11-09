use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    output::first_line,
    path::{display_path, resolve_path},
    tools::AgentContext,
};

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

    fn description(&self) -> String {
        "Write file contents".to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write. Relative to the \
                                    working directory unless absolute. `~` is \
                                    expanded; pass it through as-is."
                },
                "content": {
                    "type": "string",
                    "description": "Full file content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    /// The resolved path, spelled the way every other tool in the trace spells
    /// it. A call that arrived without one says so.
    fn trace_label(&self, arguments: &Value, cwd: &Path) -> Option<String> {
        Some(arguments.get("path").and_then(Value::as_str).map_or_else(
            || "?".to_string(),
            |path| first_line(&display_path(cwd, path)),
        ))
    }

    fn lock_path(&self) -> Option<&'static str> {
        Some("path")
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: WriteInput = serde_json::from_value(input)
            .context("invalid write input; expected { path: string, content: string }")?;

        let path = resolve_path(&ctx.cwd, &input.path);

        if let Some(parent) = path.parent()
            && parent != Path::new("")
        {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create parent directories for: {}",
                    path.display()
                )
            })?;
        }

        tokio::fs::write(&path, input.content)
            .await
            .with_context(|| format!("failed to write file: {}", path.display()))?;

        Ok(ToolResult {
            content: format!("wrote {}", path.display()),
            is_error: false,
        })
    }
}

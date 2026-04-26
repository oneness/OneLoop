use std::fs;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent::AgentContext,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head, truncation_notice},
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

    fn description(&self) -> &'static str {
        "Read file contents"
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

        if !path.exists() {
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

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read file: {}", path.display()))?;

        let truncated = truncate_head(&content, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES);
        let notice = if truncated.truncated {
            Some(truncation_notice(&truncated))
        } else {
            None
        };
        let mut final_content = truncated.content;
        if let Some(notice) = notice {
            if !final_content.ends_with('\n') && !final_content.is_empty() {
                final_content.push('\n');
            }
            final_content.push_str(&notice);
        }

        Ok(ToolResult {
            content: final_content,
            is_error: false,
        })
    }
}

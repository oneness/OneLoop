use std::fs;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{agent::context::AgentContext, output::{truncate_head, truncation_notice, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES}};

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

        let path = if path.exists() {
            path
        } else {
            // Try common extensions if the bare name doesn't exist
            let candidates = [".rs", ".ts", ".js", ".json", ".toml", ".md", ".yaml", ".yml"];
            candidates
                .iter()
                .find_map(|ext| {
                    let candidate = path.with_extension(&ext[1..]);
                    if candidate.exists() { Some(candidate) } else { None }
                })
                .ok_or_else(|| anyhow!("file does not exist: {}", path.display()))?
        };

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

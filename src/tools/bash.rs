use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{process::Command, time::{timeout, Duration}};

use crate::{agent::AgentContext, output::{truncate_tail, truncation_notice, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES}};

use super::{Tool, ToolResult};

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: BashInput = serde_json::from_value(input)
            .context("invalid bash input; expected { command: string, timeout_secs?: number }")?;

        let timeout_secs = input.timeout_secs.unwrap_or(30);
        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&input.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = timeout(Duration::from_secs(timeout_secs), command.output())
            .await
            .with_context(|| format!("bash command timed out after {} seconds", timeout_secs))??;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();
        let success = output.status.success();

        let mut combined = String::new();
        combined.push_str(&format!("command: {}\n", input.command));
        combined.push_str(&format!("exit_code: {}\n", exit_code.map_or_else(|| "terminated by signal".to_string(), |code| code.to_string())));

        if !stdout.trim().is_empty() {
            combined.push_str("stdout:\n");
            combined.push_str(&stdout);
            if !stdout.ends_with('\n') {
                combined.push('\n');
            }
        }

        if !stderr.trim().is_empty() {
            combined.push_str("stderr:\n");
            combined.push_str(&stderr);
            if !stderr.ends_with('\n') {
                combined.push('\n');
            }
        }

        let truncated = truncate_tail(&combined, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES);
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
            is_error: !success,
        })
    }
}

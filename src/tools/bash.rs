use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::agent::AgentContext;
use crate::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

use super::{Tool, ToolResult};

/// What a model's `bash` call gets when it names no timeout. A `!` command runs
/// under `BASH_TIMEOUT_SECS` in `app.rs` instead.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    timeout_secs: Option<u64>,
}

/// Runs one bash command in `cwd` and returns the transcript a model is shown,
/// so the tool and the prompt share one implementation and one output shape.
pub async fn run(command_text: &str, cwd: &Path, timeout_secs: u64) -> Result<ToolResult> {
    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(command_text)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // When the timeout drops the output() future, kill the child
        // instead of leaving it running detached.
        .kill_on_drop(true);

    let output = timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .with_context(|| format!("bash command timed out after {timeout_secs} seconds"))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();
    let success = output.status.success();

    let mut combined = String::new();
    combined.push_str(&format!("command: {command_text}\n"));
    combined.push_str(&format!(
        "exit_code: {}\n",
        exit_code.map_or_else(
            || "terminated by signal".to_string(),
            |code| code.to_string()
        )
    ));

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

    Ok(ToolResult {
        content: truncate_tail(&combined, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
        is_error: !success,
    })
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> String {
        "Execute a shell command".to_string()
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
                    "description": "Optional timeout in seconds (default 30)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &AgentContext) -> Result<ToolResult> {
        let input: BashInput = serde_json::from_value(input)
            .context("invalid bash input; expected { command: string, timeout_secs?: number }")?;
        // No command can meet a zero-second timeout, so it is a missing timeout
        // in disguise: treat it as one instead of failing every such call.
        let timeout_secs = input
            .timeout_secs
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        run(&input.command, &ctx.cwd, timeout_secs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> AgentContext {
        AgentContext {
            cwd: std::env::temp_dir(),
        }
    }

    #[tokio::test]
    async fn a_long_output_keeps_its_last_lines_and_says_so() {
        let result = run("seq 1 5000", &std::env::temp_dir(), 30).await.unwrap();

        assert!(result.content.contains("\n5000") && result.content.contains("output truncated"));
    }

    #[tokio::test]
    async fn an_output_within_the_limits_is_untouched() {
        let result = run("seq 1 500", &std::env::temp_dir(), 30).await.unwrap();

        assert!(!result.content.contains("output truncated"));
    }

    #[tokio::test]
    async fn a_zero_timeout_runs_the_command_instead_of_failing_it() {
        let result = BashTool
            .execute(json!({"command": "echo hi", "timeout_secs": 0}), &context())
            .await
            .unwrap();

        assert!(
            result.content.contains("exit_code: 0"),
            "{}",
            result.content
        );
    }
}

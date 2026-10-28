use std::{path::Path, process::Stdio};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    process::Command,
    time::{Duration, timeout},
};

use crate::{
    agent::AgentContext,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail},
};

use super::{Tool, ToolResult};

/// The tool's own default, unchanged from the literal it replaced: a model's
/// `bash` call that names no timeout gets this. A `!` command typed at the
/// prompt runs under `BASH_TIMEOUT_SECS` in `app.rs` instead, and `run` takes
/// the number as an argument so neither caller can inherit the other's.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    timeout_secs: Option<u64>,
}

/// Runs one bash command in `cwd` and returns what a model is shown, whether
/// the model asked for it or the user typed it at the prompt behind a `!`. The
/// transcript shape — `command`, `exit_code`, `stdout`, `stderr` — is therefore
/// identical on both paths.
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
        // A zero-second timeout no command can meet, so a call carrying one is
        // asking for the default and getting a guaranteed failure instead. It
        // is a missing timeout in disguise: treat it as one.
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

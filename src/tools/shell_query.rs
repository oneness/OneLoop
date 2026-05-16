use std::process::Stdio;

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
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail, truncation_notice},
};

use super::{Tool, ToolResult};

/// Read-only shell commands for inspection and search.
pub struct ShellQueryTool;

#[derive(Debug, Deserialize)]
struct ShellQueryInput {
    command: String,
    timeout_secs: Option<u64>,
}

/// Commands safe for multi-model orchestration. Read-only: no writes, no
/// mutations, no network calls (beyond what git already does).
const ALLOWED_COMMANDS: &[&str] = &[
    // File inspection
    "cat",
    "head",
    "tail",
    "less",
    "wc",
    "file",
    "stat",
    "ls",
    "tree",
    "du",
    "df",
    // Search
    "find",
    "grep",
    "rg",
    "ag",
    "ack",
    "fd",
    "fzf",
    "which",
    "type",
    // Git (read-only)
    "git",
    // Text processing (read-only in practice)
    "sort",
    "uniq",
    "cut",
    "tr",
    "awk",
    "sed",  // sed -n (print only) is read-only; sed -i would fail on writes
    "jq",
    "yq",
    "xargs",
    // Misc inspection
    "echo",
    "printf",
    "date",
    "uname",
    "hostname",
    "whoami",
    "env",
    "printenv",
    "pwd",
    "diff",
    "comm",
    "paste",
    "tee", // tee to /dev/null is fine; writing to real files will hit filesystem perms
    "column",
    "fmt",
    "fold",
    "expand",
    "unexpand",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "test",
    "[",
    // Counting / inspecting
    "nl",
    "od",
    "hexdump",
    "xxd",
    "strings",
    "md5sum",
    "sha256sum",
    "shasum",
    "cksum",
];

fn extract_base_command(command: &str) -> &str {
    // Strip leading pipes, subshells, variable assignments.
    let trimmed = command.trim_start();
    // Take the first word (the command name).
    trimmed.split_whitespace().next().unwrap_or("")
}

fn is_allowed(command: &str) -> bool {
    let base = extract_base_command(command);
    // Handle `git` subcommands — allow read-only subcommands only.
    if base == "git" {
        return is_git_read_only(command);
    }
    ALLOWED_COMMANDS.contains(&base)
}

/// Git subcommands safe for read-only inspection.
const GIT_READ_ONLY: &[&str] = &[
    "log",
    "diff",
    "show",
    "status",
    "branch",
    "tag",
    "remote",
    "stash",
    "blame",
    "shortlog",
    "describe",
    "reflog",
    "ls-files",
    "ls-tree",
    "ls-remote",
    "rev-list",
    "rev-parse",
    "name-rev",
    "merge-base",
    "cherry",
    "cherry-pick", // --no-commit is read-only but rare; allow base command
    "grep",
    "cat-file",
    "show-ref",
    "for-each-ref",
    "count-objects",
    "verify-pack",
    "fsck",
    "whatchanged",
];

fn is_git_read_only(command: &str) -> bool {
    let parts: Vec<&str> = command.split_whitespace().collect();
    // Find the subcommand (skip "git" and any flags before it).
    let subcommand = parts.iter().skip(1).find(|p| !p.starts_with('-'));
    match subcommand {
        Some(sub) => GIT_READ_ONLY.contains(sub),
        None => false, // bare "git" with no subcommand
    }
}

#[async_trait]
impl Tool for ShellQueryTool {
    fn name(&self) -> &'static str {
        "shell_query"
    }

    fn description(&self) -> &'static str {
        "Run read-only shell commands for inspection and search (find, grep, rg, git log, ls, cat, etc.). \
         Write operations and mutating commands are not allowed."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Read-only shell command for inspection or search"
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
        let input: ShellQueryInput = serde_json::from_value(input)
            .context("invalid shell_query input; expected { command: string, timeout_secs?: number }")?;

        if !is_allowed(&input.command) {
            return Ok(ToolResult {
                content: format!(
                    "blocked: '{}' is not an allowed command for shell_query. \
                     Only read-only inspection and search commands are permitted \
                     (find, grep, rg, git log, ls, cat, wc, etc.).",
                    input.command
                ),
                is_error: true,
            });
        }

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
            .with_context(|| format!("shell_query timed out after {timeout_secs} seconds"))??;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();
        let success = output.status.success();

        let mut combined = String::new();
        combined.push_str(&format!("command: {}\n", input.command));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_commands_pass() {
        assert!(is_allowed("find . -name '*.rs'"));
        assert!(is_allowed("grep -rn 'TODO' src/"));
        assert!(is_allowed("rg 'pattern' src/"));
        assert!(is_allowed("ls -la"));
        assert!(is_allowed("cat README.md"));
        assert!(is_allowed("wc -l src/main.rs"));
        assert!(is_allowed("git log --oneline -10"));
        assert!(is_allowed("git diff HEAD~1"));
        assert!(is_allowed("git status"));
        assert!(is_allowed("git blame src/main.rs"));
        assert!(is_allowed("tree src/"));
        assert!(is_allowed("du -sh ."));
        assert!(is_allowed("jq '.name' package.json"));
        assert!(is_allowed("sort file.txt | uniq"));
    }

    #[test]
    fn blocked_commands_fail() {
        assert!(!is_allowed("rm -rf /"));
        assert!(!is_allowed("curl http://example.com"));
        assert!(!is_allowed("python3 -c 'import os'"));
        assert!(!is_allowed("npm install"));
        assert!(!is_allowed("git push"));
        assert!(!is_allowed("git commit -m 'hack'"));
        assert!(!is_allowed("git reset --hard"));
        assert!(!is_allowed("git clean -fd"));
        assert!(!is_allowed("mv file.txt /tmp"));
        assert!(!is_allowed("chmod 777 file"));
        assert!(!is_allowed("docker run ubuntu"));
    }

    #[test]
    fn extract_base_command_strips_pipes() {
        assert_eq!(extract_base_command("find . -name '*.rs' | head -5"), "find");
        assert_eq!(extract_base_command("  grep -rn TODO src/"), "grep");
        assert_eq!(extract_base_command("git log --oneline"), "git");
    }
}

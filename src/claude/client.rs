//! Stateless second opinions through the locally authenticated Claude Code CLI.
//!
//! The mechanism lives in [`crate::review`]; what is Claude's own is the
//! binary and its flags, and that its system prompt travels as a flag rather
//! than folded into the prompt text.

use std::path::Path;

use anyhow::Result;

use crate::messages::Message;
use crate::review::{self, Cli, Review, SystemPrompt};

pub use crate::review::{DEFAULT_INSTRUCTION, Request};

const CLI_ARGS: &[&str] = &[
    "-p",
    "--safe-mode",
    "--no-session-persistence",
    "--tools",
    "Bash",
    "--allowedTools",
    "Bash",
    "--disallowedTools",
    "mcp__*",
    "--permission-prompts",
    "none",
    "--output-format",
    "text",
];

const CLI: Cli = Cli {
    binary: "claude",
    args: CLI_ARGS,
    system_prompt: SystemPrompt::Flag("--system-prompt"),
    name: "Claude",
    missing: "could not launch Claude Code; install `claude` and log in with your subscription first",
};

/// The name this backend reports itself under.
pub const NAME: &str = "Claude";

/// Ask Claude without resuming or persisting a Claude session.
pub async fn review(messages: &[Message], request: Request<'_>, cwd: &Path) -> Result<Review> {
    review::review(&CLI, messages, request, cwd).await
}

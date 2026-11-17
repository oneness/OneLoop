//! Stateless second opinions through the locally authenticated Antigravity CLI (agy).
//!
//! The mechanism lives in [`crate::review`]; what is Antigravity's own is the
//! binary, its flags, and the fact that it has no system-prompt flag — so the
//! prompt is handed over as text.

use std::path::Path;

use anyhow::Result;

use crate::messages::Message;
use crate::review::{self, Cli, Review, SystemPrompt};

pub use crate::review::{DEFAULT_INSTRUCTION, Request};

// CLI args for non-interactive review mode: stdin for the prompt (not `--print`,
// which demands a prompt argument and would trip the argument-size limit), raw text
// out, and tool permissions auto-approved.
const CLI_ARGS: &[&str] = &[
    "--input-format",
    "text",
    "--output-format",
    "text",
    "--dangerously-skip-permissions",
];

/// The name this backend reports itself under.
pub const NAME: &str = "Antigravity";
/// The executable used for this backend.
pub const BINARY: &str = "agy";

const CLI: Cli = Cli {
    binary: BINARY,
    args: CLI_ARGS,
    system_prompt: SystemPrompt::InPrompt,
    name: NAME,
    missing: "could not launch Antigravity CLI; install `agy` and log in with your Google account first",
};

/// Ask Antigravity as a fresh review: no conversation is resumed.
pub async fn review(messages: &[Message], request: Request<'_>, cwd: &Path) -> Result<Review> {
    review::review(&CLI, messages, request, cwd).await
}

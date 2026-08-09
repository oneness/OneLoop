use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::output;
use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;

use super::messages::Message;

/// Approximate characters per token (conservative for mixed code/prose).
/// Good enough to size a metric by; never good enough to decide on, which is
/// why nothing branches on it — the server is the authority on what fits.
const CHARS_PER_TOKEN: usize = 4;

/// Roughly how many tokens a request will carry, for the `tokens_estimated`
/// field of an `api_call` event. Approximate by construction: it exists to
/// make a log line sortable, not to predict a refusal.
pub fn estimate_tokens(messages: &[Message], system_prompt_chars: usize) -> usize {
    let message_chars: usize = messages
        .iter()
        .map(|message| match message {
            Message::User(user) => user.content.len(),
            Message::Assistant(assistant) => assistant.content.len(),
            Message::ToolCall(tool_call) => {
                tool_call.name.len() + tool_call.arguments.to_string().len()
            }
            Message::ToolResult(tool_result) => tool_result.content.len(),
        })
        .sum();

    (system_prompt_chars + message_chars) / CHARS_PER_TOKEN
}

/// Append-only metrics log, one JSON line per event.
/// File lives at `.oneloop/metrics/<session-filename>.jsonl`,
/// mirroring the session file naming for easy correlation.
pub struct Metrics {
    path: PathBuf,
}

impl Metrics {
    /// `.oneloop/sessions/2026-04-20.jsonl` → `.oneloop/metrics/2026-04-20.jsonl`
    pub fn from_session_path(session_path: &Path) -> Result<Self> {
        let project_dir = session_path
            .parent() // sessions/
            .and_then(|p| p.parent()) // .oneloop/
            .context("session path is too shallow to derive metrics directory")?;

        let metrics_dir = project_dir.join("metrics");
        fs::create_dir_all(&metrics_dir).with_context(|| {
            format!(
                "failed to create metrics directory: {}",
                metrics_dir.display()
            )
        })?;

        let filename = session_path
            .file_name()
            .context("session path has no filename")?;

        Ok(Self {
            path: metrics_dir.join(filename),
        })
    }

    /// Errors are printed to stderr, never propagated.
    pub fn log(&self, event: &str, data: Value) {
        if let Err(e) = self.try_log(event, data) {
            output::note(&format!("[metrics] {e:#}"));
        }
    }

    fn try_log(&self, event: &str, data: Value) -> Result<()> {
        let ts = Local::now().to_rfc3339();
        let mut entry = serde_json::json!({ "ts": ts, "event": event });

        if let Value::Object(data_map) = data
            && let Value::Object(ref mut entry_map) = entry
        {
            for (k, v) in data_map {
                entry_map.insert(k, v);
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open metrics file: {}", self.path.display()))?;

        serde_json::to_writer(&mut file, &entry)
            .with_context(|| format!("failed to write metrics: {}", self.path.display()))?;
        writeln!(file)
            .with_context(|| format!("failed to write metrics: {}", self.path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::messages::{ToolResultMessage, UserMessage};

    #[test]
    fn an_estimate_counts_the_prompt_and_every_message() {
        let messages = vec![
            Message::User(UserMessage {
                content: "a".repeat(40),
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "1".to_string(),
                tool_name: "read".to_string(),
                content: "b".repeat(40),
                is_error: false,
            }),
        ];

        assert_eq!(estimate_tokens(&messages, 20), 25);
    }

    #[test]
    fn an_empty_session_estimates_the_prompt_alone() {
        assert_eq!(estimate_tokens(&[], 400), 100);
    }
}

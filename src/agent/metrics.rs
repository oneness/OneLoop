use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::output;
use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;

use crate::messages::Message;

use super::session::Session;

/// Approximate bytes per token (conservative for mixed code/prose).
/// Good enough to size a metric by; never good enough to decide on, which is
/// why nothing branches on it — the server is the authority on what fits.
const BYTES_PER_TOKEN: usize = 3;

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

    (system_prompt_chars + message_chars) / BYTES_PER_TOKEN
}

/// Roughly how much of a declared context window is still free, as a
/// percentage 0–100. The same inputs as `estimate_tokens`, against the
/// window the model declares in the config. A window that fits the request
/// many times over saturates at 100 rather than overflowing.
pub fn remaining_context(
    messages: &[Message],
    system_prompt_chars: usize,
    context_window: u64,
) -> u8 {
    percent_remaining(
        estimate_tokens(messages, system_prompt_chars),
        context_window,
    )
}

/// The same gauge from a prompt-token count the server reported for the last
/// turn — the real size of the request rather than an estimate. Preferred
/// whenever a turn has run, since it is measured against the model's own
/// tokenizer instead of a bytes-per-token constant.
pub fn remaining_context_from_tokens(input_tokens: u64, context_window: u64) -> u8 {
    percent_remaining(input_tokens as usize, context_window)
}

/// `used` tokens against a declared window, clamped to 0–100 so an overfull
/// request reads empty rather than negative.
fn percent_remaining(used: usize, context_window: u64) -> u8 {
    let window = context_window.max(1) as usize;
    let fraction = 1.0 - (used as f64 / window as f64);
    ((fraction * 100.0).round() as i64).clamp(0, 100) as u8
}

/// Append-only metrics log, one JSON line per event.
/// File lives at `.oneloop/metrics/<session-filename>.jsonl`,
/// mirroring the session file naming for easy correlation.
pub struct Metrics {
    /// `None` when metrics are off — a one-shot run leaves no trace.
    path: Option<PathBuf>,
}

impl Metrics {
    /// The ledger that costs `session`: `<root>/metrics/<session file name>`,
    /// beside the conversation it belongs to. The session's root decides, so a
    /// project's own conversation and a named agent's are each costed where
    /// they live rather than in one shared file.
    ///
    /// A run that writes no conversation has nothing to cost, and gets no
    /// ledger.
    pub fn for_session(session: &Session) -> Result<Self> {
        let Some(root) = session.root() else {
            return Ok(Self { path: None });
        };

        let metrics_dir = root.join("metrics");
        fs::create_dir_all(&metrics_dir).with_context(|| {
            format!(
                "failed to create metrics directory: {}",
                metrics_dir.display()
            )
        })?;

        let filename = session
            .path()
            .file_name()
            .context("session path has no filename")?;

        Ok(Self {
            path: Some(metrics_dir.join(filename)),
        })
    }

    /// Errors are printed to stderr, never propagated.
    pub fn log(&self, event: &str, data: Value) {
        if let Err(e) = self.try_log(event, data) {
            output::note(&format!("[metrics] {e:#}"));
        }
    }

    fn try_log(&self, event: &str, data: Value) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };

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
            .open(path)
            .with_context(|| format!("failed to open metrics file: {}", path.display()))?;

        serde_json::to_writer(&mut file, &entry)
            .with_context(|| format!("failed to write metrics: {}", path.display()))?;
        writeln!(file).with_context(|| format!("failed to write metrics: {}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spec::SessionPolicy;
    use crate::messages::{ToolResultMessage, UserMessage};

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

        assert_eq!(estimate_tokens(&messages, 20), 33);
    }

    #[test]
    fn an_empty_session_estimates_the_prompt_alone() {
        assert_eq!(estimate_tokens(&[], 400), 133);
    }

    fn user_of(len: usize) -> Vec<Message> {
        vec![Message::User(UserMessage {
            content: "a".repeat(len),
        })]
    }

    #[test]
    fn a_light_session_reads_near_full() {
        assert_eq!(remaining_context(&user_of(4_000), 0, 100_000), 99);
    }

    #[test]
    fn a_heavy_session_reads_near_empty() {
        assert_eq!(remaining_context(&user_of(384_000), 0, 100_000), 0);
    }

    #[test]
    fn an_overfull_session_saturates_at_zero() {
        assert_eq!(remaining_context(&user_of(800_000), 0, 100_000), 0);
    }

    #[test]
    fn a_session_fitting_many_times_over_saturates_at_one_hundred() {
        assert_eq!(remaining_context(&user_of(400), 400, 100_000), 100);
    }

    /// A count the server reported is used as-is: same window, same clamp,
    /// no bytes-per-token approximation in the path.
    #[test]
    fn a_reported_token_count_drives_the_gauge_directly() {
        assert_eq!(remaining_context_from_tokens(444_590, 1_050_000), 58);
    }

    #[test]
    fn a_reported_token_count_over_the_window_reads_empty() {
        assert_eq!(remaining_context_from_tokens(2_000_000, 1_050_000), 0);
    }

    /// A named agent's session lives under its own root, and the metrics file
    /// follows it there: `.oneloop/agents/reviewer/sessions/x.jsonl` is costed
    /// at `.oneloop/agents/reviewer/metrics/x.jsonl`, beside the conversation it
    /// belongs to rather than in the project's own ledger.
    #[test]
    fn a_named_agents_metrics_sit_beside_its_own_session() {
        let cwd = project_dir("named-metrics");
        let session = Session::open(&cwd, SessionPolicy::Named("reviewer")).unwrap();

        let metrics = Metrics::for_session(&session).unwrap();

        let expected = cwd
            .join(".oneloop")
            .join("agents")
            .join("reviewer")
            .join("metrics")
            .join(session.path().file_name().unwrap());
        assert_eq!(metrics.path.as_deref(), Some(expected.as_path()));
        let _ = fs::remove_dir_all(&cwd);
    }

    /// A run that writes no conversation has nothing to cost, so it writes no
    /// ledger either — the same rule the session follows.
    #[test]
    fn a_one_shot_has_no_ledger() {
        let cwd = project_dir("oneshot-metrics");
        let session = Session::open(&cwd, SessionPolicy::Ephemeral { namespace: None }).unwrap();

        let metrics = Metrics::for_session(&session).unwrap();

        assert!(metrics.path.is_none());
        let _ = fs::remove_dir_all(&cwd);
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oneloop-metrics-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn metrics_with_no_ledger_record_nothing() {
        let metrics = Metrics { path: None };

        assert!(metrics.try_log("api_call", serde_json::json!({})).is_ok());
    }
}

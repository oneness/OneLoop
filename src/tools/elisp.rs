use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    process::Command,
    time::{Duration, timeout},
};

use crate::{
    agent::AgentContext,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head},
};

use super::{Tool, ToolResult};

const DEFAULT_TIMEOUT_SECS: u64 = 10;

pub struct ElispTool;

#[derive(Debug, Deserialize)]
struct ElispInput {
    expression: String,
    timeout_secs: Option<u64>,
}

/// Emacsclient prints strings in Lisp syntax. Encoding the value in Emacs
/// gives us an unambiguous transport while preserving arbitrary buffer text.
fn transport_expression(expression: &str) -> String {
    format!(
        "(let* ((oneloop-value {expression}) \
         (oneloop-text (if (stringp oneloop-value) oneloop-value \
         (prin1-to-string oneloop-value)))) \
         (base64-encode-string \
         (encode-coding-string oneloop-text 'utf-8) t))"
    )
}

fn decode_output(output: &[u8]) -> Result<String> {
    let printed = String::from_utf8_lossy(output);
    let encoded = printed.trim();
    let Some(encoded) = encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        bail!("emacsclient returned an unexpected value: {encoded}");
    };
    let decoded = STANDARD
        .decode(encoded)
        .context("emacsclient returned invalid encoded output")?;
    String::from_utf8(decoded).context("Emacs returned non-UTF-8 text")
}

#[async_trait]
impl Tool for ElispTool {
    fn name(&self) -> &'static str {
        "elisp"
    }

    fn description(&self) -> String {
        "Evaluate Emacs Lisp in the running Emacs server. Load the emacs skill before use."
            .to_string()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "Emacs Lisp expression to evaluate. Pass plain Lisp, without shell escaping."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional client timeout in seconds; this cannot stop Lisp already running in Emacs."
                }
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
        let input: ElispInput = serde_json::from_value(input).context(
            "invalid elisp input; expected { expression: string, timeout_secs?: number }",
        )?;
        let timeout_secs = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let mut command = Command::new("emacsclient");
        command
            .arg("--eval")
            .arg(transport_expression(&input.expression))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // This stops only the client. Emacs may still be evaluating Lisp.
            .kill_on_drop(true);

        let output = timeout(Duration::from_secs(timeout_secs), command.output())
            .await
            .with_context(|| {
                format!(
                    "emacsclient timed out after {timeout_secs} seconds; Lisp already running in Emacs may continue"
                )
            })?
            .context("failed to run emacsclient; ensure it is installed and an Emacs server is running")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let message = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Ok(ToolResult {
                content: truncate_head(message, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES),
                is_error: true,
            });
        }

        Ok(ToolResult {
            content: truncate_head(
                &decode_output(&output.stdout)?,
                DEFAULT_MAX_BYTES,
                DEFAULT_MAX_LINES,
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_preserves_plain_lisp_without_shell_quoting() {
        let expression = transport_expression("(buffer-name (window-buffer (selected-window)))");
        assert!(expression.contains("(let* ((oneloop-value (buffer-name"));
    }

    #[test]
    fn transport_encodes_multibyte_text_as_utf8_bytes() {
        let expression = transport_expression("\"λ\"");
        assert!(expression.contains("(encode-coding-string oneloop-text 'utf-8)"));
    }

    #[test]
    fn encoded_multiline_buffer_text_is_decoded_verbatim() {
        let encoded = STANDARD.encode("first\nsecond λ");
        let printed = format!("\"{encoded}\"\n");
        assert_eq!(
            decode_output(printed.as_bytes()).unwrap(),
            "first\nsecond λ"
        );
    }

    #[test]
    fn unexpected_client_output_is_an_error() {
        assert!(decode_output(b"not-a-lisp-string\n").is_err());
    }
}

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::agent::messages::ToolCall;

/// Request sent to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::agent::messages::Message>,
    pub tools: Vec<crate::tools::ToolDefinition>,
    /// Override the provider's configured model for this request only.
    pub model_override: Option<String>,
}

/// Response from a provider.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Endpoint name. Borrowed rather than `&'static str`: names come from
    /// `~/.oneloop/endpoints.json` and are only known at runtime.
    fn name(&self) -> &str;
    fn model(&self) -> String;
    /// Tokens this endpoint will accept in one request. Compaction aims to
    /// stay under it, so a wrong value here is not a tuning knob — too high
    /// and the request is rejected outright with nothing to fall back on.
    fn context_window(&self) -> usize;
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;
}

/// A non-2xx response, carrying the status so callers can tell a temporary
/// failure from a request that will never succeed.
#[derive(Debug)]
pub struct ProviderHttpError {
    pub status: reqwest::StatusCode,
    pub message: String,
    pub provider: String,
}

impl std::fmt::Display for ProviderHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} request failed ({}): {}",
            self.provider, self.status, self.message
        )
    }
}

impl std::error::Error for ProviderHttpError {}

impl ProviderHttpError {
    /// Whether sending the identical request again could plausibly work.
    ///
    /// Client errors are the request's fault and are deterministic: a body
    /// over the context limit is over it on every attempt. Retrying burns
    /// minutes of local inference to arrive at the same place, and offering
    /// a paid endpoint as the "fallback" turns a local misconfiguration
    /// into a bill. Rate limiting and timeouts are the exceptions.
    pub fn is_retryable(&self) -> bool {
        if self.status.is_client_error() {
            return matches!(
                self.status,
                reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
            );
        }
        true
    }
}

/// Whether an error from `complete` is worth another attempt. Anything that
/// is not a recognised HTTP status is assumed transient — that is the old
/// behaviour, and a connection reset really can succeed on retry.
pub fn is_retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ProviderHttpError>()
        .map(ProviderHttpError::is_retryable)
        .unwrap_or(true)
}

pub mod chat;
pub mod registry;

// Re-export key types for convenience.
pub use chat::ChatProvider;
pub use registry::ProviderRegistry;

/// Send a prepared provider request, check the status, and return the raw
/// body text. Non-2xx responses become errors carrying the provider's own
/// error message.
async fn send_and_read(request: reqwest::RequestBuilder, provider: &str) -> Result<String> {
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to send request to {provider}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("failed to read {provider} response body"))?;
    if !status.is_success() {
        return Err(ProviderHttpError {
            status,
            message: extract_error_message(&text),
            provider: provider.to_string(),
        }
        .into());
    }
    Ok(text)
}

/// Extract a concise error message from a provider JSON error response.
/// Falls back to truncating the raw text at 200 characters.
fn extract_error_message(raw: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        // Try common shapes: {"error": {"message": "..."}} or {"error": "string"}
        if let Some(error) = val.get("error") {
            if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
            if let Some(msg) = error.as_str() {
                return msg.to_string();
            }
        }
    }
    // Last resort: truncate the raw text.
    let truncated = crate::output::truncate_at_char_boundary(raw, 200);
    if truncated.len() < raw.len() {
        format!("{truncated}…")
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::extract_error_message;

    #[test]
    fn error_truncation_handles_multibyte_text() {
        // 199 ASCII bytes, then a two-byte character straddling byte 200 —
        // the old byte-index slice panicked here.
        let raw = format!("{}écurité and more trailing text", "x".repeat(199));
        let msg = extract_error_message(&raw);
        assert!(msg.ends_with('…'));
        assert!(msg.starts_with("xxx"));
    }
}

#[cfg(test)]
mod retry_tests {
    use super::{ProviderHttpError, is_retryable};
    use reqwest::StatusCode;

    fn err(status: StatusCode) -> anyhow::Error {
        ProviderHttpError {
            status,
            message: "boom".to_string(),
            provider: "local".to_string(),
        }
        .into()
    }

    #[test]
    fn a_rejected_request_is_not_retried() {
        // The case that motivated this: a body over the context limit is
        // over it on every attempt, and the retries cost minutes of local
        // inference before offering a paid endpoint as the cure.
        assert!(!is_retryable(&err(StatusCode::BAD_REQUEST)));
        assert!(!is_retryable(&err(StatusCode::UNAUTHORIZED)));
        assert!(!is_retryable(&err(StatusCode::NOT_FOUND)));
    }

    #[test]
    fn rate_limits_and_timeouts_are_retried() {
        assert!(is_retryable(&err(StatusCode::TOO_MANY_REQUESTS)));
        assert!(is_retryable(&err(StatusCode::REQUEST_TIMEOUT)));
    }

    #[test]
    fn server_errors_are_retried() {
        assert!(is_retryable(&err(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(is_retryable(&err(StatusCode::SERVICE_UNAVAILABLE)));
    }

    #[test]
    fn a_non_http_error_stays_retryable() {
        // Connection resets really can succeed on a second attempt; only a
        // recognised status downgrades that assumption.
        assert!(is_retryable(&anyhow::anyhow!("connection reset")));
    }
}

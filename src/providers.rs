//! Providers: the endpoints OneLoop calls.
//!
//! What belongs to the place — URL, key, connection pool — lives here; what
//! varies per model lives on the model.

use anyhow::{Context, Result};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};

use crate::agent::messages::ToolCall;

/// One endpoint: where to send a request and how to be let in.
#[derive(Debug)]
pub struct Provider {
    /// Its name in the config file, and in anything shown to the user.
    pub name: String,
    base_url: String,
    /// `None` is the local server, which needs no credentials.
    api_key: Option<String>,
    /// One pool per endpoint, not per model: two models from one provider
    /// are two ids sent to the same place.
    client: reqwest::Client,
}

impl Provider {
    pub fn new(name: &str, base_url: &str, api_key: Option<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .with_context(|| format!("failed to build HTTP client for {name}"))?;

        Ok(Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
        })
    }

    /// Authorized if this provider needs it.
    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let request = self.client.post(url);
        // A local server needs no credentials, and sending an empty bearer
        // token makes some servers reject the request outright.
        match &self.api_key {
            Some(key) => request.header("Authorization", format!("Bearer {key}")),
            None => request,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::agent::messages::Message>,
    pub tools: Vec<crate::tools::ToolDefinition>,
    /// The `model:` directive: something the provider hosts but the config
    /// never listed, for this request only.
    pub model_id_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Carries the status so callers can tell a temporary failure from a
/// request that will never succeed.
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

/// Whether sending the identical request again could plausibly work.
///
/// Client errors are deterministic: a body over the context limit is over it
/// on every attempt, and offering a paid endpoint as the "fallback" turns a
/// local misconfiguration into a bill. Rate limits and timeouts are the
/// exceptions; anything not a recognised status is assumed transient.
pub fn is_retryable(error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<ProviderHttpError>() else {
        return true;
    };
    if error.status.is_client_error() {
        return matches!(
            error.status,
            reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
        );
    }
    true
}

pub mod chat;

/// Non-2xx becomes an error carrying the provider's own message.
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

/// Falls back to truncating the raw text at 200 characters.
fn extract_error_message(raw: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        // {"error": {"message": "..."}} or {"error": "string"}
        if let Some(error) = val.get("error") {
            if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
            if let Some(msg) = error.as_str() {
                return msg.to_string();
            }
        }
    }
    let truncated = crate::output::truncate_at_char_boundary(raw, 200);
    if truncated.len() < raw.len() {
        format!("{truncated}…")
    } else {
        raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{Provider, ProviderHttpError, extract_error_message, is_retryable};
    use reqwest::StatusCode;

    fn http_error(status: StatusCode) -> anyhow::Error {
        ProviderHttpError {
            status,
            message: "boom".to_string(),
            provider: "local".to_string(),
        }
        .into()
    }

    fn provider(base_url: &str, api_key: Option<&str>) -> Provider {
        Provider::new("p", base_url, api_key.map(String::from)).expect("client must build")
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let request = provider("http://u/v1/", None).post("chat/completions");
        let request = request.build().unwrap();
        assert_eq!(request.url().as_str(), "http://u/v1/chat/completions");
    }

    #[test]
    fn a_key_is_sent_as_a_bearer_token() {
        let request = provider("http://u", Some("secret"))
            .post("chat/completions")
            .build()
            .unwrap();
        assert_eq!(request.headers()["Authorization"], "Bearer secret");
    }

    #[test]
    fn a_provider_without_a_key_sends_no_authorization() {
        // An empty bearer token makes some local servers reject outright.
        let request = provider("http://u", None)
            .post("chat/completions")
            .build()
            .unwrap();
        assert!(request.headers().get("Authorization").is_none());
    }

    #[test]
    fn error_truncation_handles_multibyte_text() {
        // 199 ASCII bytes, then a two-byte character straddling byte 200 —
        // the old byte-index slice panicked here.
        let raw = format!("{}écurité and more trailing text", "x".repeat(199));
        let msg = extract_error_message(&raw);
        assert!(msg.ends_with('…'));
        assert!(msg.starts_with("xxx"));
    }

    #[test]
    fn a_rejected_request_is_not_retried() {
        // The case that motivated this: a body over the context limit cost
        // minutes of local inference per attempt, then offered a paid
        // endpoint as the cure.
        assert!(!is_retryable(&http_error(StatusCode::BAD_REQUEST)));
        assert!(!is_retryable(&http_error(StatusCode::UNAUTHORIZED)));
        assert!(!is_retryable(&http_error(StatusCode::NOT_FOUND)));
    }

    #[test]
    fn rate_limits_and_timeouts_are_retried() {
        assert!(is_retryable(&http_error(StatusCode::TOO_MANY_REQUESTS)));
        assert!(is_retryable(&http_error(StatusCode::REQUEST_TIMEOUT)));
    }

    #[test]
    fn server_errors_are_retried() {
        assert!(is_retryable(&http_error(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(is_retryable(&http_error(StatusCode::SERVICE_UNAVAILABLE)));
    }

    #[test]
    fn a_non_http_error_stays_retryable() {
        // Connection resets really can succeed on a second attempt; only a
        // recognised status downgrades that assumption.
        assert!(is_retryable(&anyhow::anyhow!("connection reset")));
    }
}

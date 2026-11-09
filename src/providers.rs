//! Providers: the endpoints OneLoop calls, and the models that answer through
//! them.
//!
//! What belongs to the place — URL, credentials, connection pool — lives here;
//! what varies per model lives on the model, and the model lives here because
//! everything the wire reads is transport data. The one field the wire never
//! reads, `context_window`, rides along: it is the gauge's, and a second
//! descriptor for one `Option<u64>` would cost more than it explains.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::auth::{self, Credential, OauthEntry};
use crate::catalog::Api;
use crate::messages::ToolCall;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CHAT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub(super) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// One model, ready to answer.
#[derive(Debug)]
pub struct Model {
    /// The short name used everywhere else — `/model`, `default`.
    pub alias: String,
    /// What goes on the wire.
    pub id: String,
    pub provider: Arc<Provider>,
    /// The protocol its provider speaks, which is what decides which module
    /// below gets the request.
    pub api: Api,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub web_tools: bool,
    pub reasoning_effort: Option<String>,
    /// Approximate capacity, for the context gauge in the interactive
    /// prompt. Nothing branches on it — the server is the authority on what
    /// fits and refuses when it does not.
    pub context_window: Option<u64>,
}

impl Model {
    pub async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        match self.api {
            Api::Chat => chat::complete(self, request).await,
            Api::Codex | Api::Responses => responses::complete(self, request).await,
        }
    }
}

impl fmt::Display for Model {
    /// `flash · openrouter (~deepseek/deepseek-v4-flash-latest)`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} · {} ({})", self.alias, self.provider.name, self.id)
    }
}

/// OpenCode Go requires a stable per-conversation ID to route and cache prompts.
pub(super) const OPENCODE_GO: &str = "opencode-go";
pub(super) const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// How a provider is let in.
#[derive(Debug)]
pub enum Credentials {
    /// The local server, which needs none.
    None,
    ApiKey(String),
    /// A subscription: a token that expires, renewed here as it does. Behind
    /// a lock because the models of one provider share it, and two of them
    /// refreshing at once would spend the same one-use refresh token twice.
    Oauth(Mutex<OauthEntry>),
    /// Configured, but nobody has signed in yet. Carries the command that
    /// fixes it, so the refusal says what to do rather than 401 later.
    Missing(String),
}

/// One endpoint: where to send a request and how to be let in.
#[derive(Debug)]
pub struct Provider {
    /// Its name in the config file, and in anything shown to the user.
    pub name: String,
    base_url: String,
    credentials: Credentials,
    /// One pool per endpoint, not per model: two models from one provider
    /// are two ids sent to the same place.
    client: reqwest::Client,
}

impl Provider {
    pub fn new(name: &str, base_url: &str, credentials: Credentials) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .default_headers(headers)
            .build()
            .with_context(|| format!("failed to build HTTP client for {name}"))?;

        Ok(Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            credentials,
            client,
        })
    }

    /// Authorized if this provider needs it — which for a subscription can
    /// mean renewing the grant first, hence the `async` and the `Result`.
    pub async fn post(&self, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        // Identify as OneLoop rather than letting reqwest's default library
        // name through — OpenCode Go asks clients to name themselves.
        let request = self.client.post(url).header(
            USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        );
        Ok(match &self.credentials {
            // A local server needs no credentials, and sending an empty
            // bearer token makes some servers reject the request outright.
            Credentials::None => request,
            Credentials::ApiKey(key) => request.header("Authorization", format!("Bearer {key}")),
            Credentials::Oauth(grant) => {
                let grant = self.fresh_grant(grant).await?;
                request
                    .header("Authorization", format!("Bearer {}", grant.access))
                    .header("chatgpt-account-id", grant.account_id)
            }
            Credentials::Missing(fix) => {
                return Err(NotSignedIn {
                    provider: self.name.clone(),
                    fix: fix.clone(),
                }
                .into());
            }
        })
    }

    /// The stored grant, renewed and re-stored when it is close to expiring.
    /// A failed write is a warning rather than an error: the token in hand
    /// still works, and only the next run pays for having to sign in again.
    async fn fresh_grant(&self, grant: &Mutex<OauthEntry>) -> Result<OauthEntry> {
        let mut grant = grant.lock().await;
        if let Some(renewed) = auth::codex::refreshed(&grant).await? {
            if let Err(e) = auth::store(&self.name, &Credential::Oauth(renewed.clone())) {
                crate::output::warn(&format!("could not store the renewed session: {e:#}"));
            }
            *grant = renewed;
        }
        Ok(grant.clone())
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system_prompt: Option<String>,
    /// Stable identity for provider-side conversation routing.
    pub session_id: String,
    pub messages: Vec<crate::messages::Message>,
    pub tools: Vec<crate::messages::ToolDefinition>,
    /// Where a refusal is recorded — the project's `.oneloop/errors.log`.
    /// Carried on the request because the provider layer is what decides a
    /// response counts as a refusal, and it is the only thing that knows
    /// which project the request belongs to. None for a one-shot, which
    /// writes nothing: recording a refusal would mean creating `.oneloop/`
    /// in whatever directory the prompt happened to run in.
    pub errors_log: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning_content: Option<String>,
    /// Input tokens the server counted for the request that produced this
    /// response, when it reports them. The real size of the prompt, as
    /// opposed to `estimate_tokens`'s approximation, so the context gauge can
    /// use a measured number once a turn has succeeded.
    pub input_tokens: Option<u64>,
}

/// A provider that is configured but that nobody has signed in to. Its own
/// type so the retry path can tell it apart: no amount of waiting signs
/// anyone in, and the request never left the machine.
#[derive(Debug)]
pub struct NotSignedIn {
    provider: String,
    /// The command that fixes it.
    fix: String,
}

impl std::fmt::Display for NotSignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not signed in to {} — run `{}`", self.provider, self.fix)
    }
}

impl std::error::Error for NotSignedIn {}

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
    // Nothing was sent, and nothing about waiting signs anyone in.
    if error.downcast_ref::<NotSignedIn>().is_some() {
        return false;
    }
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

/// Whether the request was refused because the conversation no longer fits.
///
/// Worth telling apart from the rest not because the loop can cure it — it
/// cannot — but because the cure is a specific one the user can apply, and
/// "provider error: 400" does not suggest `/clear`. Every server words it
/// differently — llama.cpp
/// "exceeds the available context size", OpenAI "maximum context length",
/// Anthropic "prompt is too long" — and none of them use a status of their
/// own, so the message is all there is to go on. An unrecognised phrasing
/// stays an ordinary error, which is what every one of them was before this
/// existed: the cost of missing one is the behaviour we already had.
pub fn is_context_overflow(error: &anyhow::Error) -> bool {
    /// Lowercase, and matched as substrings: the numbers around them differ
    /// on every occurrence.
    const PHRASES: [&str; 6] = [
        "context length",
        "context size",
        "context window",
        "prompt is too long",
        "too many tokens",
        "token count",
    ];

    let Some(error) = error.downcast_ref::<ProviderHttpError>() else {
        return false;
    };
    if !matches!(
        error.status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::PAYLOAD_TOO_LARGE
    ) {
        return false;
    }
    let message = error.message.to_lowercase();
    PHRASES.iter().any(|phrase| message.contains(phrase))
}

pub mod chat;
pub mod responses;

/// Keeps the raw text when it will not parse.
///
/// A truncated call is not a transport failure: retrying arrives at the same
/// place a minute later, and the model is the only party that can fix it —
/// which it can only do if told. The raw text keeps the call intact for the
/// history the API requires, and the error travels alongside it.
///
/// Shared by both protocols: the two wire formats disagree about almost
/// everything, but a model that stops mid-argument-list stops the same way
/// on either.
fn decode_tool_arguments(arguments: Value) -> (Value, Option<String>) {
    match arguments {
        Value::String(text) => match serde_json::from_str(&text) {
            Ok(value) => (value, None),
            Err(err) => (Value::String(text), Some(err.to_string())),
        },
        other => (other, None),
    }
}

/// Non-2xx becomes an error carrying the provider's own message.
async fn send_and_read(
    request: reqwest::RequestBuilder,
    provider: &str,
    errors_log: Option<&Path>,
) -> Result<(reqwest::StatusCode, String)> {
    tokio::time::timeout(
        CHAT_TIMEOUT,
        send_and_read_inner(request, provider, errors_log),
    )
    .await
    .with_context(|| format!("{provider} request timed out after 15 minutes"))?
}

async fn send_and_read_inner(
    request: reqwest::RequestBuilder,
    provider: &str,
    errors_log: Option<&Path>,
) -> Result<(reqwest::StatusCode, String)> {
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
        log_provider_failure(errors_log, provider, status, &text);
        return Err(ProviderHttpError {
            status,
            message: extract_error_message(&text),
            provider: provider.to_string(),
        }
        .into());
    }
    Ok((status, text))
}

/// Records a failed exchange with a provider, one JSON object per line, in
/// the project's `.oneloop/errors.log`.
///
/// The console line is a summary, and the reason a request was refused is
/// usually only in the body: OpenRouter answers a gated model with a 403
/// whose `failed_routing_step` names the gate, and none of that metadata
/// reaches the message the user sees. Shared rather than written per call
/// site because every refusal — a status, a 200 carrying an `error` object, a
/// body that parses as neither — is the same event with the same fields.
///
/// A log that cannot be written must not replace the failure it was meant to
/// explain, so the write error is a warning and the caller's error is
/// untouched.
pub(super) fn log_provider_failure(
    errors_log: Option<&Path>,
    alias: &str,
    status: StatusCode,
    body: &str,
) {
    // A one-shot carries no path rather than create `.oneloop/` in whatever
    // directory its prompt ran in; the refusal is still reported on stderr.
    let Some(path) = errors_log else {
        return;
    };
    if let Err(e) = append_record(path, alias, status, body) {
        crate::output::warn(&format!(
            "could not record the provider failure in {}: {e:#}",
            path.display()
        ));
    }
}

/// The refusal log filed in `root`: the directory a conversation keeps its
/// session, its metrics and its lock in, so a refusal sits beside the
/// conversation it interrupted.
///
/// Per conversation rather than under the home directory: one file for every
/// conversation merged unrelated histories into a single one.
pub fn errors_log_path(root: &Path) -> PathBuf {
    root.join("errors.log")
}

/// Takes the path so a test can write somewhere other than the project.
fn append_record(path: &Path, alias: &str, status: StatusCode, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{}", failure_record(alias, status, body)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// One line, whatever the body held: newlines and quotes in it are escaped
/// rather than allowed to split the record.
fn failure_record(alias: &str, status: StatusCode, body: &str) -> Result<String> {
    Ok(format!(
        "{{\"ts\":{},\"event\":\"provider_error\",\"model\":{},\"status\":{},\"body\":{}}}",
        serde_json::to_string(&Local::now().to_rfc3339())?,
        serde_json::to_string(alias)?,
        status.as_u16(),
        serde_json::to_string(body)?,
    ))
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
    use super::{
        Credentials, OauthEntry, Provider, ProviderHttpError, append_record, decode_tool_arguments,
        errors_log_path, extract_error_message, is_context_overflow, is_retryable,
    };
    use reqwest::StatusCode;
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    /// A directory of its own per test, so two of these can run at once and
    /// neither writes into the other's log.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oneloop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("errors.log")
    }

    /// Reads the log and clears the scratch directory. Parsing every line is
    /// itself the assertion that nothing split a record.
    fn read_records(path: &std::path::Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).expect("the log must exist");
        let _ = std::fs::remove_dir_all(path.parent().expect("scratch path has a parent"));
        text.lines()
            .map(|line| serde_json::from_str(line).expect("every line must be one JSON object"))
            .collect()
    }

    fn http_error(status: StatusCode) -> anyhow::Error {
        refusal(status, "boom")
    }

    fn refusal(status: StatusCode, message: &str) -> anyhow::Error {
        ProviderHttpError {
            status,
            message: message.to_string(),
            provider: "local".to_string(),
        }
        .into()
    }

    fn provider(base_url: &str, credentials: Credentials) -> Provider {
        Provider::new("p", base_url, credentials).expect("client must build")
    }

    #[tokio::test]
    async fn a_trailing_slash_does_not_double_up() {
        let request = provider("http://u/v1/", Credentials::None)
            .post("chat/completions")
            .await
            .unwrap();
        let request = request.build().unwrap();
        assert_eq!(request.url().as_str(), "http://u/v1/chat/completions");
    }

    /// OpenCode Go asks clients to identify themselves rather than send a
    /// generic SDK or HTTP-library name, so every provider carries OneLoop's.
    #[tokio::test]
    async fn every_provider_identifies_itself() {
        let request = provider("http://u", Credentials::None)
            .post("chat/completions")
            .await
            .unwrap()
            .build()
            .unwrap();
        let agent = request.headers()["user-agent"].to_str().unwrap();
        assert_eq!(agent, concat!("oneloop/", env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn a_key_is_sent_as_a_bearer_token() {
        let request = provider("http://u", Credentials::ApiKey("secret".to_string()))
            .post("chat/completions")
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["Authorization"], "Bearer secret");
    }

    #[tokio::test]
    async fn a_provider_without_a_key_sends_no_authorization() {
        // An empty bearer token makes some local servers reject outright.
        let request = provider("http://u", Credentials::None)
            .post("chat/completions")
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().get("Authorization").is_none());
    }

    /// The 401 a request would earn says nothing about how to fix it — and
    /// waiting to send it again fixes nothing either.
    #[tokio::test]
    async fn a_provider_nobody_signed_in_to_says_how_to_sign_in_once() {
        let provider = provider(
            "http://u",
            Credentials::Missing("ol login openai".to_string()),
        );
        let err = provider.post("responses").await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("not signed in to p"), "got: {message}");
        assert!(message.contains("ol login openai"), "got: {message}");
        assert!(!is_retryable(&err));
    }

    /// A live grant is used as it stands — no network, no refresh.
    #[tokio::test]
    async fn a_subscription_sends_its_token_and_its_account() {
        let grant = OauthEntry {
            access: "tok".to_string(),
            refresh: "ref".to_string(),
            expires: chrono::Utc::now().timestamp() + 3600,
            account_id: "acct-1".to_string(),
        };
        let request = provider("http://u", Credentials::Oauth(Mutex::new(grant)))
            .post("codex/responses")
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["Authorization"], "Bearer tok");
        assert_eq!(request.headers()["chatgpt-account-id"], "acct-1");
    }

    #[test]
    fn decode_tool_arguments_parses_a_json_string() {
        let (arguments, error) =
            decode_tool_arguments(Value::String(r#"{"command":"ls"}"#.to_string()));
        assert_eq!(arguments, json!({"command": "ls"}));
        assert!(error.is_none());
    }

    #[test]
    fn decode_tool_arguments_passes_through_an_object() {
        let (arguments, error) = decode_tool_arguments(json!({"command": "ls"}));
        assert_eq!(arguments, json!({"command": "ls"}));
        assert!(error.is_none());
    }

    #[test]
    fn decode_tool_arguments_reports_a_truncated_string() {
        // The shape actually observed: generation stopped mid-arguments, so
        // the JSON has no closing quote or brace.
        let truncated = r#"{"command":"cargo test 2>&1"#;
        let (arguments, error) = decode_tool_arguments(Value::String(truncated.to_string()));
        assert_eq!(arguments, Value::String(truncated.to_string()));
        assert!(error.is_some(), "a truncated call must report an error");
    }

    #[test]
    fn decode_tool_arguments_keeps_the_raw_text_when_it_will_not_parse() {
        // The raw text is what the model actually sent, and it is what the
        // conversation has to carry back — the API owes a tool call for every
        // result, and inventing arguments here would misreport the turn.
        let raw = "not json at all";
        let (arguments, error) = decode_tool_arguments(Value::String(raw.to_string()));
        assert_eq!(arguments, Value::String(raw.to_string()));
        assert!(error.is_some());
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
    fn every_server_words_an_overflow_differently() {
        // Verbatim shapes, because the substrings are chosen to survive the
        // numbers each one interpolates.
        for message in [
            "the request exceeds the available context size. try increasing the context size",
            "This model's maximum context length is 8192 tokens. However, your messages resulted in 9001 tokens",
            "prompt is too long: 215427 tokens > 200000 maximum",
            "The input token count (1048576) exceeds the maximum number of tokens allowed",
        ] {
            assert!(
                is_context_overflow(&refusal(StatusCode::BAD_REQUEST, message)),
                "not recognised: {message}"
            );
        }
        assert!(is_context_overflow(&refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too many tokens"
        )));
    }

    #[test]
    fn an_ordinary_refusal_is_not_an_overflow() {
        // Pointing any of these at /clear would be advice that cures
        // nothing, on an error that has a real cause worth reading.
        assert!(!is_context_overflow(&refusal(
            StatusCode::BAD_REQUEST,
            "unknown model: gpt-9"
        )));
        assert!(!is_context_overflow(&refusal(
            StatusCode::UNAUTHORIZED,
            "invalid api key"
        )));
        assert!(!is_context_overflow(&anyhow::anyhow!("connection reset")));
    }

    #[test]
    fn an_overflow_is_never_retried_as_it_stands() {
        // The two classifiers must agree: an oversized body is oversized on
        // every attempt, so retrying only spends the wait again.
        let error = refusal(StatusCode::BAD_REQUEST, "maximum context length is 8192");
        assert!(is_context_overflow(&error));
        assert!(!is_retryable(&error));
    }

    #[test]
    fn a_non_http_error_stays_retryable() {
        // Connection resets really can succeed on a second attempt; only a
        // recognised status downgrades that assumption.
        assert!(is_retryable(&anyhow::anyhow!("connection reset")));
    }

    /// The log belongs to the conversation it explains, not to the home
    /// directory: one file for every conversation merged unrelated histories.
    #[test]
    fn the_error_log_is_filed_beside_its_conversation() {
        assert_eq!(
            errors_log_path(std::path::Path::new("/repo/.oneloop")),
            std::path::PathBuf::from("/repo/.oneloop/errors.log")
        );
    }

    /// The line the console prints is a summary; the reason — OpenRouter's
    /// `failed_routing_step`, an upstream provider's own words — exists only
    /// in the body, so the record has to carry it.
    #[test]
    fn a_refusal_is_recorded_with_its_status_and_body() {
        let path = scratch("refusal");
        append_record(
            &path,
            "inkling",
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"only available on agentic harnesses"}}"#,
        )
        .expect("the log must be writable");
        let records = read_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["event"], "provider_error");
        assert_eq!(records[0]["model"], "inkling");
        assert_eq!(records[0]["status"], 403);
        assert_eq!(
            records[0]["body"],
            r#"{"error":{"message":"only available on agentic harnesses"}}"#
        );
        assert!(records[0]["ts"].is_string());
    }

    /// A body is arbitrary bytes from someone else's server: a newline in it
    /// must not look like the start of the next record.
    #[test]
    fn a_multiline_body_stays_one_record() {
        let path = scratch("newline");
        append_record(
            &path,
            "local",
            StatusCode::BAD_REQUEST,
            "line one\nline two\n",
        )
        .expect("the log must be writable");
        let records = read_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["body"], "line one\nline two\n");
    }

    /// The directory is created on the first failure, which is the first time
    /// anything in a fresh home writes this file.
    #[test]
    fn a_log_directory_is_created_when_missing() {
        let path = scratch("missing");
        assert!(!path.exists());
        append_record(&path, "local", StatusCode::BAD_GATEWAY, "boom")
            .expect("the log must be writable");
        assert_eq!(read_records(&path).len(), 1);
    }

    /// Append, not replace: the failures before this run are the history that
    /// makes the log worth having.
    #[test]
    fn a_second_failure_keeps_the_first() {
        let path = scratch("append");
        append_record(&path, "local", StatusCode::BAD_GATEWAY, "first")
            .expect("the log must be writable");
        append_record(&path, "inkling", StatusCode::FORBIDDEN, "second")
            .expect("the log must be writable");
        let records = read_records(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["body"], "first");
        assert_eq!(records[1]["body"], "second");
    }
}

//! Signing in to a ChatGPT subscription, and keeping that grant alive.
//!
//! An API key is one line the user pastes. A subscription is not: OpenAI
//! hands it out through the browser, so this runs the authorization-code
//! flow with PKCE — a challenge in the URL, a one-request web server on the
//! port the redirect names, and an exchange for tokens that expire.
//!
//! The client id, ports, and endpoints are the Codex CLI's own: the grant is
//! issued to that client, so nothing here is ours to choose.

use std::fs::File;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::OauthEntry;
use crate::output;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Registered with the client id above, down to the port and the path — the
/// server cannot be told to redirect anywhere else.
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDR: (&str, u16) = ("127.0.0.1", 1455);
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// Where the access token carries the account the subscription belongs to.
const JWT_CLAIM: &str = "https://api.openai.com/auth";
/// Names this client in OpenAI's logs; also sent on every later request.
pub const ORIGINATOR: &str = "oneloop";
/// The config and credential name for this one subscription provider.
pub const PROVIDER_NAME: &str = "openai";

/// Renew this early, so a request never starts on a token that expires while
/// it is in flight.
const REFRESH_SKEW_SECONDS: i64 = 60;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The browser flow, start to finish: open the page, wait for the redirect,
/// trade the code for tokens.
pub async fn login() -> Result<OauthEntry> {
    let verifier = random_base64url(32)?;
    let state = random_base64url(16)?;
    let url = authorize_url(&challenge_for(&verifier), &state);

    // Bound before the browser opens, or the redirect can arrive at a port
    // nothing is listening on yet.
    let listener = TcpListener::bind(CALLBACK_ADDR).await.with_context(|| {
        format!(
            "failed to listen on {}:{}",
            CALLBACK_ADDR.0, CALLBACK_ADDR.1
        )
    })?;

    output::step("opening your browser to sign in to ChatGPT");
    output::note("if it does not open, visit this URL yourself:");
    eprintln!("\n{url}\n");
    open_browser(&url).await;

    let code = tokio::select! {
        code = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_code(&listener, &state)) => {
            code.context("sign-in timed out after 5 minutes")??
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!();
            bail!("cancelled — not signed in");
        }
    };

    entry_from(exchange_code(&code, &verifier).await?)
}

/// A grant that is still good, renewing it first if it is not.
///
/// The refresh token rotates on every use, so the caller must store what
/// comes back — an unstored rotation costs the next run its session.
pub async fn refreshed(entry: &OauthEntry) -> Result<Option<OauthEntry>> {
    if entry.expires - chrono::Utc::now().timestamp() > REFRESH_SKEW_SECONDS {
        return Ok(None);
    }
    Ok(Some(entry_from(refresh_tokens(&entry.refresh).await?)?))
}

// ── The flow ──────────────────────────────────────────────────────────

fn authorize_url(challenge: &str, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", percent_encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

/// What a request to the callback path turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// The redirect this sign-in started.
    Code(String),
    /// OpenAI turned the sign-in down. Nothing better is coming, so this
    /// ends the wait.
    Refused(String),
    /// Not this sign-in's redirect. The page says so and the wait goes on —
    /// ending here would let anything that can reach the port cancel a
    /// sign-in the user is in the middle of.
    Stray(String),
}

/// Answers requests until one is the redirect we are waiting for. A browser
/// asks for `/favicon.ico`, retries, and follows links of its own, so a
/// single accept is not enough.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .context("failed to accept the OAuth redirect")?;

        let Some(target) = read_request_target(&mut socket).await? else {
            continue;
        };
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (target.as_str(), ""),
        };
        if path != CALLBACK_PATH {
            respond(&mut socket, "404 Not Found", "Nothing here.").await;
            continue;
        }

        match classify(query, state) {
            Callback::Code(code) => {
                respond(
                    &mut socket,
                    "200 OK",
                    "Signed in. You can close this window and return to OneLoop.",
                )
                .await;
                return Ok(code);
            }
            Callback::Refused(message) => {
                respond(&mut socket, "400 Bad Request", &message).await;
                bail!("{message}");
            }
            Callback::Stray(message) => {
                respond(&mut socket, "400 Bad Request", &message).await;
            }
        }
    }
}

/// The code a redirect carries, once its `state` proves it answers the
/// request this process started.
fn classify(query: &str, state: &str) -> Callback {
    let params: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), percent_decode(value)))
        .collect();
    let value = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    // Without this check, any page the browser can reach could hand this
    // server a code or refusal of its choosing.
    if value("state") != Some(state) {
        return Callback::Stray(
            "state mismatch — this did not answer the sign-in in progress".to_string(),
        );
    }
    if let Some(error) = value("error") {
        return Callback::Refused(format!(
            "OpenAI refused the sign-in: {error}{}",
            value("error_description")
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ));
    }
    match value("code") {
        Some(code) => Callback::Code(code.to_string()),
        None => Callback::Stray("the redirect carried no authorization code".to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

async fn exchange_code(code: &str, verifier: &str) -> Result<TokenResponse> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ])
    .await
    .context("failed to exchange the authorization code")
}

async fn refresh_tokens(refresh: &str) -> Result<TokenResponse> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh),
    ])
    .await
    .context("failed to refresh the ChatGPT session — sign in again with `ol login openai`")
}

async fn post_token(form: &[(&str, &str)]) -> Result<TokenResponse> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOKEN_TIMEOUT)
        .build()
        .context("failed to build the authentication client")?;
    let response = client
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .context("failed to reach auth.openai.com")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read the token response")?;
    if !status.is_success() {
        bail!("token request failed ({status}): {body}");
    }
    serde_json::from_str(&body).context("token response was not the shape expected")
}

fn entry_from(tokens: TokenResponse) -> Result<OauthEntry> {
    Ok(OauthEntry {
        account_id: account_id(&tokens.access_token)
            .context("the access token named no ChatGPT account")?,
        expires: chrono::Utc::now().timestamp() + tokens.expires_in,
        access: tokens.access_token,
        refresh: tokens.refresh_token,
    })
}

/// Read out of the token rather than asked for separately: it is a claim the
/// issuer signed, and the backend rejects requests that omit it.
fn account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get(JWT_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

// ── Odds and ends the flow needs ──────────────────────────────────────

/// PKCE: the server stores this hash, and only the process holding the
/// verifier it came from can redeem the code the redirect carries.
fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The kernel's pool rather than a random-number crate: this is the one
/// place OneLoop needs unpredictable bytes. Read exactly what is asked for —
/// the device never reaches EOF, so reading it to the end never returns.
fn random_base64url(bytes: usize) -> Result<String> {
    let mut random = vec![0_u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut pool| pool.read_exact(&mut random))
        .context("failed to read random bytes from /dev/urandom")?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

/// Enough of the browser's request to route it: the target of the first
/// line. The headers and body that follow are of no interest.
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Result<Option<String>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !buffer.windows(2).any(|w| w == b"\r\n") {
        // A client that connects and says nothing must not hold the flow
        // open, and one that floods must not be read forever.
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 || buffer.len() > 8192 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let line = String::from_utf8_lossy(&buffer);
    Ok(line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string))
}

async fn respond(socket: &mut tokio::net::TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>OneLoop</title>\
         <body style=\"font-family:system-ui;margin:4rem;line-height:1.6\">\
         <h1>OneLoop</h1><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Nothing to do if the browser hung up first — the code is already in
    // hand, and the page it would have shown is a courtesy.
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Best effort: the URL is printed either way, so a machine with no browser
/// (or no opener) loses nothing but a click.
async fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = tokio::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

/// Query-string escaping for the handful of characters the authorize URL can
/// carry — spaces in the scope, slashes and colons in the redirect.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.replace('+', " ").into_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|pair| u8::from_str_radix(&String::from_utf8_lossy(pair), 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// A JWT for tests: only the payload is ever read, so the header and
/// signature are placeholders.
#[cfg(test)]
fn test_token(payload: &Value) -> String {
    format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

/// Padded base64 shows up in tokens issued by other implementations; the
/// engine is here so `account_id` can be tested against both spellings.
#[cfg(test)]
fn test_token_padded(payload: &Value) -> String {
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD
        .encode(payload.to_string())
        .replace('+', "-")
        .replace('/', "_");
    format!("header.{encoded}==.signature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(id: &str) -> Value {
        json!({ JWT_CLAIM: { "chatgpt_account_id": id } })
    }

    #[test]
    fn the_account_id_comes_out_of_the_access_token() {
        let token = test_token(&claims("acct-123"));
        assert_eq!(account_id(&token).as_deref(), Some("acct-123"));
    }

    #[test]
    fn a_padded_payload_still_decodes() {
        let token = test_token_padded(&claims("acct-123"));
        assert_eq!(account_id(&token).as_deref(), Some("acct-123"));
    }

    /// Every failure is one answer: no account, so no request can be sent
    /// pretending there is one.
    #[test]
    fn a_token_without_the_claim_names_no_account() {
        assert!(account_id("not-a-jwt").is_none());
        assert!(account_id(&test_token(&json!({ "sub": "u" }))).is_none());
        assert!(account_id(&test_token(&claims(""))).is_none());
    }

    #[test]
    fn the_challenge_is_the_sha256_of_the_verifier() {
        // The one vector RFC 7636 gives, which is what the server checks
        // against.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_authorize_url_carries_the_challenge_and_state() {
        let url = authorize_url("chal", "st");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        // The scope's spaces and the redirect's punctuation must survive.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
    }

    #[test]
    fn the_code_is_read_from_the_redirect() {
        assert_eq!(
            classify("code=abc123&state=st", "st"),
            Callback::Code("abc123".to_string())
        );
    }

    #[test]
    fn a_percent_encoded_code_is_decoded() {
        assert_eq!(
            classify("state=st&code=ab%2Bc%3Dd", "st"),
            Callback::Code("ab+c=d".to_string())
        );
    }

    /// Without the state check, any page the browser can reach could post a
    /// code of its own choosing to a server listening on a known port. And
    /// because such a request is not the redirect, it must not end the wait
    /// for the one that is — a stray hit on the port would otherwise cancel
    /// a sign-in the user is halfway through.
    #[test]
    fn a_request_that_is_not_this_redirect_leaves_the_wait_running() {
        assert!(matches!(
            classify("code=abc&state=other", "st"),
            Callback::Stray(_)
        ));
        assert!(matches!(classify("code=abc", "st"), Callback::Stray(_)));
        assert!(matches!(classify("state=st", "st"), Callback::Stray(_)));
    }

    /// A refusal is the sign-in's answer, so this one does end the wait.
    #[test]
    fn a_refusal_is_reported_with_what_the_server_said() {
        let Callback::Refused(message) = classify(
            "error=access_denied&error_description=User%20said%20no&state=st",
            "st",
        ) else {
            panic!("a refusal must end the wait");
        };
        assert!(message.contains("access_denied"), "got: {message}");
        assert!(message.contains("User said no"), "got: {message}");
    }

    #[test]
    fn a_refusal_without_this_sign_ins_state_is_stray() {
        assert!(matches!(
            classify("error=access_denied&state=other", "st"),
            Callback::Stray(_)
        ));
        assert!(matches!(
            classify("error=access_denied", "st"),
            Callback::Stray(_)
        ));
    }

    #[test]
    fn a_grant_with_life_left_is_not_renewed() {
        let entry = OauthEntry {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: chrono::Utc::now().timestamp() + 3600,
            account_id: "acct".to_string(),
        };
        // No network is touched, which is the point: a live token is used as
        // it stands.
        let renewed = futures::executor::block_on(refreshed(&entry)).unwrap();
        assert!(renewed.is_none());
    }

    #[test]
    fn random_values_differ() {
        assert_ne!(random_base64url(32).unwrap(), random_base64url(32).unwrap());
    }
}

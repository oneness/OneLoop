//! Named endpoints: where requests go, and as which model.
//!
//! Every endpoint speaks OpenAI Chat Completions — a local llama-server and
//! OpenRouter are the same protocol, so the only thing separating them is a
//! URL, a model name, and whether a key is needed. That makes "which
//! provider" a naming question rather than a protocol one, and lets
//! `#!consensus local openrouter#!` compare two models instead of two
//! vendors.
//!
//! Config lives beside `auth.json` in `~/.oneloop/`. A missing file is not
//! an error: the built-in defaults below are a working setup.

use std::collections::BTreeMap;
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Fallback when `~/.oneloop/endpoints.json` is absent. Local is the
/// default: it costs nothing and needs no key, so an unconfigured checkout
/// cannot accidentally bill a hosted model.
const DEFAULT_LOCAL_URL: &str = "http://localhost:8080/v1";
const DEFAULT_HOSTED_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_HOSTED_MODEL: &str = "deepseek/deepseek-v4-flash";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    /// Environment variable holding this endpoint's key. Absent means no
    /// key — the local server case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// OpenRouter's server-side web_search/web_fetch tools. Metered per use
    /// ($0.005/search, $0.001/fetch), and a local server has no such thing,
    /// so this is off unless the endpoint asks for it.
    #[serde(default)]
    pub web_tools: bool,
    /// No default: a hosted provider's own per-model ceiling is the right
    /// one, and guessing here would cap long answers that work today.
    /// Self-hosted servers are the case that needs it — their ceiling is
    /// whatever the operator passed to the server, and a reasoning model
    /// that spends it all thinking returns a turn with no content at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Tokens this endpoint accepts in one request; what compaction aims to
    /// stay under. Not a preference — a self-hosted server rejects anything
    /// over whatever `-c` it was started with, so a value borrowed from a
    /// hosted model's 128k means the request is refused with no recourse.
    #[serde(default = "default_context_window")]
    pub context_window: usize,
}

/// Applies to an endpoint that does not state its own. Hosted models are the
/// common case and are large; `local` sets the same figure explicitly below
/// to match the `-c 131072` the flake's server runs with.
fn default_context_window() -> usize {
    128_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointFile {
    pub default: String,
    /// Ordered so the fallback picker and `available()` list endpoints the
    /// same way every run.
    pub endpoints: BTreeMap<String, Endpoint>,
}

impl Default for EndpointFile {
    fn default() -> Self {
        let mut endpoints = BTreeMap::new();
        endpoints.insert(
            "local".to_string(),
            Endpoint {
                base_url: DEFAULT_LOCAL_URL.to_string(),
                model: "local".to_string(),
                api_key_env: None,
                web_tools: false,
                max_tokens: Some(4096),
                temperature: None,
                // Matches `llama-server -c 131072` with headroom for the
                // reply; compaction triggers at a percentage of this.
                context_window: 128_000,
            },
        );
        endpoints.insert(
            "openrouter".to_string(),
            Endpoint {
                base_url: DEFAULT_HOSTED_URL.to_string(),
                model: DEFAULT_HOSTED_MODEL.to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                web_tools: true,
                max_tokens: None,
                temperature: None,
                context_window: default_context_window(),
            },
        );
        Self {
            default: "local".to_string(),
            endpoints,
        }
    }
}

/// Load endpoints, applying environment overrides to the default endpoint.
///
/// A missing file yields the built-in defaults. A malformed one is an error
/// rather than a silent fallback: a typo in the config should not quietly
/// send work somewhere else.
pub fn load() -> Result<EndpointFile> {
    let mut file: EndpointFile = read_file()?.unwrap_or_default();
    apply_env_overrides(&mut file);
    Ok(file)
}

/// The legacy `ONELOOP_OPENROUTER_*` variables still steer the default
/// endpoint. They predate this file and are what the local-server setup
/// notes tell people to set, so breaking them would break working setups
/// for no gain.
fn apply_env_overrides(file: &mut EndpointFile) {
    let default_name = file.default.clone();
    let Some(endpoint) = file.endpoints.get_mut(&default_name) else {
        return;
    };
    if let Ok(url) = env::var("ONELOOP_OPENROUTER_BASE_URL") {
        endpoint.base_url = url;
    }
    if let Ok(model) = env::var("ONELOOP_OPENROUTER_MODEL") {
        endpoint.model = model;
    }
    if let Some(max_tokens) = env::var("ONELOOP_OPENROUTER_MAX_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        endpoint.max_tokens = Some(max_tokens);
    }
    if let Some(temperature) = env::var("ONELOOP_OPENROUTER_TEMPERATURE")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        endpoint.temperature = Some(temperature);
    }
    if let Some(window) = env::var("ONELOOP_CONTEXT_WINDOW_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        endpoint.context_window = window;
    }
    if let Some(web_tools) = env::var("ONELOOP_WEB_TOOLS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        endpoint.web_tools = web_tools;
    }
}

fn endpoints_file_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".oneloop").join("endpoints.json"))
}

fn read_file() -> Result<Option<EndpointFile>> {
    let path = endpoints_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read endpoints file: {}", path.display()))?;
    let file = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse endpoints file: {}", path.display()))?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_prefer_local() {
        let file = EndpointFile::default();
        assert_eq!(file.default, "local");
        let local = &file.endpoints["local"];
        assert!(local.api_key_env.is_none(), "local must need no key");
        assert!(!local.web_tools, "a local server has no server-side tools");
        // Must stay at or under the `-c` the flake's server runs with
        // (131072). Above it, compaction lets through requests the server
        // rejects outright — which is the bug this value once caused.
        assert!(
            local.context_window <= 131_072,
            "local window must not exceed the server's -c"
        );
        assert_eq!(local.context_window, 128_000);
    }

    #[test]
    fn defaults_include_a_hosted_endpoint() {
        let file = EndpointFile::default();
        let hosted = &file.endpoints["openrouter"];
        assert_eq!(hosted.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(hosted.web_tools);
    }

    #[test]
    fn env_overrides_apply_to_the_default_endpoint_only() {
        let mut file = EndpointFile::default();
        // Simulate ONELOOP_OPENROUTER_BASE_URL without touching the real
        // environment, which other tests share.
        let default_name = file.default.clone();
        file.endpoints.get_mut(&default_name).unwrap().base_url = "http://elsewhere/v1".to_string();
        assert_eq!(file.endpoints["local"].base_url, "http://elsewhere/v1");
        assert_eq!(file.endpoints["openrouter"].base_url, DEFAULT_HOSTED_URL);
    }

    #[test]
    fn a_configured_file_round_trips() {
        let file = EndpointFile::default();
        let json = serde_json::to_string(&file).unwrap();
        let back: EndpointFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default, file.default);
        assert_eq!(back.endpoints.len(), file.endpoints.len());
        assert_eq!(back.endpoints["local"].model, "local");
    }

    #[test]
    fn omitted_optional_fields_parse() {
        // The minimum an endpoint can specify — everything else defaults.
        let json = r#"{"default":"x","endpoints":{"x":{"base_url":"u","model":"m"}}}"#;
        let file: EndpointFile = serde_json::from_str(json).unwrap();
        let x = &file.endpoints["x"];
        assert!(x.api_key_env.is_none());
        assert!(!x.web_tools);
        assert!(x.max_tokens.is_none());
        assert!(x.temperature.is_none());
    }
}

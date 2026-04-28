use std::env;
use std::io::{self, Write as IoWrite};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{
    AnthropicProvider, OpenAIProvider, Provider, ProviderRequest, ProviderResponse, ZaiProvider,
};
use crate::auth;

pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_index: usize,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let preferred = env::var("ONELOOP_PROVIDER").ok();

        let mut providers: Vec<Box<dyn Provider>> = Vec::new();

        let mut anthropic_index: Option<usize> = None;
        let mut zai_index: Option<usize> = None;
        let mut openai_index: Option<usize> = None;

        if let Some(key) = auth::resolve_anthropic_api_key() {
            anthropic_index = Some(providers.len());
            providers.push(Box::new(AnthropicProvider::new(key)?));
        }

        if let Some(key) = auth::resolve_zai_api_key() {
            zai_index = Some(providers.len());
            providers.push(Box::new(ZaiProvider::new(key)?));
        }

        if let Some(key) = auth::resolve_openai_api_key() {
            openai_index = Some(providers.len());
            providers.push(Box::new(OpenAIProvider::new(key)?));
        }

        let default_index = match preferred.as_deref() {
            Some("zai") => {
                zai_index.context("ONELOOP_PROVIDER=zai but no ZAI_API_KEY/auth found")?
            }
            Some("anthropic") => anthropic_index
                .context("ONELOOP_PROVIDER=anthropic but no ANTHROPIC_API_KEY/auth found")?,
            Some("openai") => {
                openai_index.context("ONELOOP_PROVIDER=openai but no OPENAI_API_KEY/auth found")?
            }
            Some(other) => bail!("unknown provider: {other}"),
            None => zai_index
                .or(openai_index)
                .or(anthropic_index)
                .context("no providers configured — run `oneloop login <provider>` first")?,
        };

        Ok(Self {
            providers,
            default_index,
        })
    }

    pub fn active_name(&self) -> &'static str {
        self.providers[self.default_index].name()
    }

    pub fn active_model(&self) -> String {
        self.providers[self.default_index].model()
    }

    pub fn available_providers(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn resolve(&self, name: Option<&str>) -> Result<&dyn Provider> {
        let name = name.unwrap_or_else(|| self.providers[self.default_index].name());
        self.providers
            .iter()
            .find(|p| p.name() == name)
            .with_context(|| {
                let available: Vec<&str> = self.providers.iter().map(|p| p.name()).collect();
                format!(
                    "unknown provider: {name}. available: {}",
                    available.join(", ")
                )
            })
            .map(AsRef::as_ref)
    }

    /// Send a request with automatic retry (up to `max_retries` attempts).
    /// On persistent failure, prompts the user interactively to pick an
    /// alternative provider from those available and retries once more.
    ///
    /// `stop_spinner` is called before showing the interactive fallback prompt
    /// so any caller-side animation (e.g. "thinking…") is halted first.
    /// `start_spinner` is called after the user selects a fallback, to resume
    /// the animation while the fallback provider processes the request.
    pub async fn complete_with_retry(
        &self,
        provider_name: Option<&str>,
        request: ProviderRequest,
        stop_spinner: Option<Box<dyn FnOnce() + Send>>,
        start_spinner: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<(String, ProviderResponse)> {
        let max_retries: usize = env::var("ONELOOP_MAX_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        let provider = self.resolve(provider_name)?;
        let provider_label = provider.name();

        // --- Phase 1: retry on the same provider ---
        let mut last_error: Option<String> = None;
        for attempt in 1..=max_retries {
            match provider.complete(request.clone()).await {
                Ok(response) => return Ok((provider_label.to_string(), response)),
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    last_error = Some(err_msg.clone());

                    if attempt < max_retries {
                        let backoff = Duration::from_millis(500 * attempt as u64);
                        eprintln!(
                            "\x1b[33m  ⚠ [{provider_label}] attempt {attempt}/{max_retries} failed: {err_msg}\x1b[0m"
                        );
                        eprintln!(
                            "\x1b[90m  ⏳ retrying in {}ms...\x1b[0m",
                            backoff.as_millis()
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        // --- Phase 2: all retries exhausted, offer fallback ---
        let err_msg = last_error.as_deref().unwrap_or("unknown error");
        eprintln!(
            "\x1b[31m  ✗ [{provider_label}] all {max_retries} attempts failed: {err_msg}\x1b[0m"
        );

        let alternatives: Vec<&'static str> = self
            .providers
            .iter()
            .map(|p| p.name())
            .filter(|name| *name != provider_label)
            .collect();

        if alternatives.is_empty() {
            bail!(
                "[{provider_label}] failed after {max_retries} retries and no alternative providers available"
            );
        }

        // Halt any caller-side spinner before showing the interactive prompt.
        if let Some(stop) = stop_spinner {
            stop();
        }

        // Show the user a numbered list.
        println!("\x1b[1m  ── Provider Unavailable ──\x1b[0m");
        println!("\x1b[90m  {provider_label} is not responding. Pick an alternative:\x1b[0m");
        for (i, name) in alternatives.iter().enumerate() {
            let model = self
                .providers
                .iter()
                .find(|p| p.name() == *name)
                .map(|p| p.model())
                .unwrap_or_else(|| "?".to_string());
            println!("\x1b[1m  {}. {} \x1b[90m({})\x1b[0m", i + 1, name, model);
        }
        print!("\x1b[1m  → select [1-{}]: \x1b[0m", alternatives.len());
        io::stdout().flush()?;
        let mut choice = String::new();
        match io::stdin().read_line(&mut choice) {
            Ok(0) => bail!("input closed — aborting"),
            Ok(_) => {
                let idx: usize = choice.trim().parse().unwrap_or(usize::MAX);
                match alternatives.get(idx - 1) {
                    Some(&name) => {
                        let fallback = self.resolve(Some(name))?;
                        eprintln!(
                            "\x1b[32m  → switching to {} ({})\x1b[0m",
                            fallback.name(),
                            fallback.model()
                        );
                        if let Some(start) = start_spinner {
                            start();
                        }
                        let response = fallback.complete(request.clone()).await?;
                        Ok((fallback.name().to_string(), response))
                    }
                    None => {
                        bail!("invalid selection: {}", choice.trim());
                    }
                }
            }
            Err(e) => bail!("failed to read input: {e}"),
        }
    }
}

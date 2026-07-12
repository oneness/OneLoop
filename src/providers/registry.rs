use std::env;
use std::io::{self, Write as IoWrite};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{
    AnthropicProvider, OpenAIProvider, OpenRouterProvider, Provider, ProviderRequest,
    ProviderResponse,
};
use crate::auth::{self, AuthProvider};

pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_index: usize,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let preferred = env::var("ONELOOP_PROVIDER").ok();
        let auth = auth::load();

        let mut providers: Vec<Box<dyn Provider>> = Vec::new();

        let mut anthropic_index: Option<usize> = None;
        let mut openai_index: Option<usize> = None;
        let mut openrouter_index: Option<usize> = None;

        if let Some(key) = auth.api_key(AuthProvider::Anthropic) {
            anthropic_index = Some(providers.len());
            providers.push(Box::new(AnthropicProvider::new(key)?));
        }

        if let Some(key) = auth.api_key(AuthProvider::OpenAi) {
            openai_index = Some(providers.len());
            providers.push(Box::new(OpenAIProvider::new(key)?));
        }

        if let Some(key) = auth.api_key(AuthProvider::OpenRouter) {
            openrouter_index = Some(providers.len());
            providers.push(Box::new(OpenRouterProvider::new(key)?));
        }

        let default_index = match preferred.as_deref() {
            Some("openrouter") => openrouter_index
                .context("ONELOOP_PROVIDER=openrouter but no OPENROUTER_API_KEY/auth found")?,
            Some("anthropic") => anthropic_index
                .context("ONELOOP_PROVIDER=anthropic but no ANTHROPIC_API_KEY/auth found")?,
            Some("openai") => {
                openai_index.context("ONELOOP_PROVIDER=openai but no OPENAI_API_KEY/auth found")?
            }
            Some(other) => bail!("unknown provider: {other}"),
            None => openrouter_index
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

    pub async fn complete_once(
        &self,
        provider_name: &str,
        request: ProviderRequest,
    ) -> Result<ProviderResponse> {
        self.resolve(Some(provider_name))?.complete(request).await
    }

    pub fn validate_provider(&self, provider_name: &str) -> Result<()> {
        self.resolve(Some(provider_name)).map(|_| ())
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
        println!(
            "\x1b[90m  {provider_label} is not responding. Pick an alternative (Enter to abort):\x1b[0m"
        );
        for (i, name) in alternatives.iter().enumerate() {
            let model = self
                .providers
                .iter()
                .find(|p| p.name() == *name)
                .map(|p| p.model())
                .unwrap_or_else(|| "?".to_string());
            println!("\x1b[1m  {}. {} \x1b[90m({})\x1b[0m", i + 1, name, model);
        }
        print!(
            "\x1b[1m  → select [1-{}] or Enter to abort: \x1b[0m",
            alternatives.len()
        );
        io::stdout().flush()?;

        // Spawn the blocking stdin read on a blocking thread so we can race it
        // against Ctrl+C. This ensures the user can abort the selection prompt
        // with Ctrl+C instead of being forced to pick a provider.
        let choice = tokio::select! {
            res = tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                io::stdin().read_line(&mut buf).map(|_| buf)
            }) => res,
            _ = tokio::signal::ctrl_c() => {
                println!();
                bail!("aborted — no provider selected");
            }
        };

        let choice = match choice {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) => bail!("failed to read input: {e}"),
            Err(e) => bail!("input thread failed: {e}"),
        };

        let trimmed = choice.trim();

        // Empty input (just pressed Enter) means abort.
        if trimmed.is_empty() {
            bail!("aborted — no provider selected");
        }

        let idx: usize = trimmed.parse().unwrap_or(usize::MAX);
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
                bail!("invalid selection: {trimmed}");
            }
        }
    }
}

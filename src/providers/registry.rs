use std::env;
use std::io::{self, IsTerminal, Write as IoWrite};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{
    AnthropicProvider, OpenAIProvider, OpenRouterProvider, Provider, ProviderRequest,
    ProviderResponse,
};
use crate::auth::{self, AuthProvider};
use crate::output::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

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

    /// The model a named provider would use — for metrics and logging.
    pub fn model_for(&self, name: &str) -> String {
        self.resolve(Some(name))
            .map(Provider::model)
            .unwrap_or_else(|_| "?".to_string())
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
        let max_retries: usize = crate::config::env_or("ONELOOP_MAX_RETRIES", 3);

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
                            "{YELLOW}  ⚠ [{provider_label}] attempt {attempt}/{max_retries} failed: {err_msg}{RESET}"
                        );
                        eprintln!(
                            "{DIM}  ⏳ retrying in {}ms...{RESET}",
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
            "{RED}  ✗ [{provider_label}] all {max_retries} attempts failed: {err_msg}{RESET}"
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

        // The picker below reads stdin; in non-interactive contexts (piped
        // input, scripts) there is nobody to answer it — fail instead.
        if !io::stdin().is_terminal() {
            bail!("[{provider_label}] failed after {max_retries} retries: {err_msg}");
        }

        // Halt any caller-side spinner before showing the interactive prompt.
        if let Some(stop) = stop_spinner {
            stop();
        }

        // Show the user a numbered list.
        println!("{BOLD}  ── Provider Unavailable ──{RESET}");
        println!(
            "{DIM}  {provider_label} is not responding. Pick an alternative (Enter to abort):{RESET}"
        );
        for (i, name) in alternatives.iter().enumerate() {
            let model = self
                .providers
                .iter()
                .find(|p| p.name() == *name)
                .map(|p| p.model())
                .unwrap_or_else(|| "?".to_string());
            println!("{BOLD}  {}. {} {DIM}({}){RESET}", i + 1, name, model);
        }
        print!(
            "{BOLD}  → select [1-{}] or Enter to abort: {RESET}",
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

        let selected = trimmed
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| alternatives.get(index));
        let Some(&name) = selected else {
            bail!("invalid selection: {trimmed}");
        };

        let fallback = self.resolve(Some(name))?;
        eprintln!(
            "{GREEN}  → switching to {} ({}){RESET}",
            fallback.name(),
            fallback.model()
        );
        if let Some(start) = start_spinner {
            start();
        }
        let response = fallback.complete(request.clone()).await?;
        Ok((fallback.name().to_string(), response))
    }
}

use std::env;
use std::io::{self, IsTerminal, Write as IoWrite};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{ChatProvider, Provider, ProviderRequest, ProviderResponse, is_retryable};
use crate::auth;
use crate::catalog;
use crate::output::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

pub struct ProviderRegistry {
    providers: Vec<Box<dyn Provider>>,
    default_index: usize,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let catalog = catalog::load()?;
        let auth = auth::load();

        // Every configured model is registered, with or without a key — a
        // local server needs none, and refusing to register it would put the
        // default out of reach.
        let mut providers: Vec<Box<dyn Provider>> = Vec::new();
        for model in catalog.models {
            let api_key = model
                .api_key_env
                .as_deref()
                .and_then(|var| auth.api_key_for(var));
            providers.push(Box::new(ChatProvider::new(model, api_key)?));
        }

        let position_of = |name: &str| providers.iter().position(|p| p.name() == name);
        // ONELOOP_MODEL picks a model for this run; ONELOOP_PROVIDER is the
        // name it had before models and providers were separated.
        let requested = env::var("ONELOOP_MODEL")
            .or_else(|_| env::var("ONELOOP_PROVIDER"))
            .ok();
        let wanted = requested.as_deref().unwrap_or(&catalog.default);
        let default_index = position_of(wanted).with_context(|| {
            let available: Vec<&str> = providers.iter().map(|p| p.name()).collect();
            format!("no model named {wanted}. configured: {}", available.join(", "))
        })?;

        Ok(Self {
            providers,
            default_index,
        })
    }

    pub fn active_name(&self) -> &str {
        self.providers[self.default_index].name()
    }

    pub fn active_model(&self) -> String {
        self.providers[self.default_index].model()
    }

    /// Context window of the model a request would use. Compaction needs
    /// this before building the request, so it is resolved by name here
    /// rather than read from a global.
    pub fn context_window_for(&self, name: Option<&str>) -> usize {
        self.resolve(name)
            .map(Provider::context_window)
            .unwrap_or_else(|_| self.providers[self.default_index].context_window())
    }

    pub fn available_providers(&self) -> Vec<&str> {
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
                    let retryable = is_retryable(&e);
                    last_error = Some(err_msg.clone());

                    // A rejected request is rejected identically next time.
                    // Retrying spends minutes of local inference to reach the
                    // same place, and the fallback prompt below would offer a
                    // paid endpoint as the cure for a local misconfiguration.
                    if !retryable {
                        bail!("[{provider_label}] {err_msg}");
                    }

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

        let alternatives: Vec<&str> = self
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

        let fallback = self
            .prompt_fallback_choice(provider_label, &alternatives)
            .await?;
        eprintln!(
            "{GREEN}  → switching to {} ({}){RESET}",
            fallback.name(),
            fallback.model()
        );
        if let Some(start) = start_spinner {
            start();
        }
        let response = fallback.complete(request).await?;
        Ok((fallback.name().to_string(), response))
    }

    /// Show a numbered menu of alternative providers and read the user's
    /// pick, racing stdin against Ctrl+C so the prompt can be aborted.
    async fn prompt_fallback_choice(
        &self,
        failed: &str,
        alternatives: &[&str],
    ) -> Result<&dyn Provider> {
        println!("{BOLD}  ── Provider Unavailable ──{RESET}");
        println!("{DIM}  {failed} is not responding. Pick an alternative (Enter to abort):{RESET}");
        for (i, name) in alternatives.iter().enumerate() {
            println!(
                "{BOLD}  {}. {} {DIM}({}){RESET}",
                i + 1,
                name,
                self.model_for(name)
            );
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

        self.resolve(Some(name))
    }
}

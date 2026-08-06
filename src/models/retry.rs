//! What to do when a model does not answer: retry it, then offer another.
//!
//! Beside the registry rather than in `providers` because the cure is a
//! different model, which only the registry knows about.

use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Result, bail};

use super::{Model, ModelRegistry};
use crate::output::{BOLD, DIM, GREEN, RED, RESET, YELLOW};
use crate::providers::{ProviderRequest, ProviderResponse, is_retryable};

impl ModelRegistry {
    /// Retries, then offers another model. Returns the alias that answered,
    /// which is not always the one asked for.
    ///
    /// The spinner callbacks let the caller halt its animation before the
    /// prompt appears and resume it once the fallback is chosen.
    pub async fn complete_with_retry(
        &self,
        alias: Option<&str>,
        request: ProviderRequest,
        stop_spinner: Option<Box<dyn FnOnce() + Send>>,
        start_spinner: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<(String, ProviderResponse)> {
        let max_retries: usize = crate::config::env_or("ONELOOP_MAX_RETRIES", 3);

        let model = self.resolve(alias)?;
        let label = model.alias.as_str();

        let mut last_error: Option<String> = None;
        for attempt in 1..=max_retries {
            match model.complete(request.clone()).await {
                Ok(response) => return Ok((label.to_string(), response)),
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    last_error = Some(err_msg.clone());

                    // Rejected once is rejected identically next time.
                    if !is_retryable(&e) {
                        bail!("[{label}] {err_msg}");
                    }

                    if attempt < max_retries {
                        let backoff = Duration::from_millis(500 * attempt as u64);
                        eprintln!(
                            "{YELLOW}  ⚠ [{label}] attempt {attempt}/{max_retries} failed: {err_msg}{RESET}"
                        );
                        eprintln!("{DIM}  ⏳ retrying in {}ms...{RESET}", backoff.as_millis());
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        let err_msg = last_error.as_deref().unwrap_or("unknown error");
        eprintln!("{RED}  ✗ [{label}] all {max_retries} attempts failed: {err_msg}{RESET}");

        let alternatives: Vec<&Model> = self
            .models
            .iter()
            .filter(|model| model.alias != label)
            .collect();

        if alternatives.is_empty() {
            bail!("[{label}] failed after {max_retries} retries and no other model is configured")
        }

        // Piped input and scripts have nobody to answer the picker.
        if !io::stdin().is_terminal() {
            bail!("[{label}] failed after {max_retries} retries: {err_msg}");
        }

        if let Some(stop) = stop_spinner {
            stop();
        }

        println!("{BOLD}  ── Model Unavailable ──{RESET}");
        println!("{DIM}  {label} is not responding. Pick another:{RESET}");
        let fallback = self.get(&self.pick(&alternatives).await?)?;

        eprintln!("{GREEN}  → switching to {fallback}{RESET}");
        if let Some(start) = start_spinner {
            start();
        }
        let response = fallback.complete(request).await?;
        Ok((fallback.alias.clone(), response))
    }
}
